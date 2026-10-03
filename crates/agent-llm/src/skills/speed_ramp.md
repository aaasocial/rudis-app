Retiming a clip — constant speed and speed ramps.

Rudis has TWO ways to change a clip's playback rate, and they are the same
underlying property, so the last one you set wins:

- **constant speed** — `set_clip_properties` with `speed`. One multiplier for
  the whole clip. `2.0` = twice as fast, `0.5` = half speed. Range 0.1-10.
- **a speed RAMP** — `set_keyframes` with `property: "speed"`. Each key sets
  the playback RATE at that moment, and the rate varies between keys.

Setting `speed` on a clip CLEARS any ramp on it. Setting a speed track
REPLACES the constant. Reverse playback (negative speed) does not exist in
Rudis; do not offer it.

Four things the tool schemas cannot tell you.

## 1. Below 1.0x you cannot invent frames — say so before you ship judder

Rudis has NO optical-flow frame interpolation (what Premiere calls Optical
Flow time interpolation and Resolve calls Optical Flow / Speed Warp). Slowing
a clip REPEATS its existing frames.

At 30fps source: 0.5x shows each frame twice, 0.25x shows each frame four
times, 0.1x shows each frame ten times. Under about 0.5x the judder is
obvious on any moving subject.

So:
- 1.0x down to ~0.5x: fine, ship it.
- 0.5x down to ~0.25x: works, looks stepped on motion. SAY SO in your reply —
  one sentence, e.g. "this is 0.25x, so each source frame holds for four
  frames and fast motion will look stepped."
- below 0.25x: tell the user before doing it, and offer the alternatives that
  actually exist here — a shorter, less extreme slow section, or holding on a
  single frame instead of crawling through many.

Never silently deliver judder and call the task done.

## 2. Detached audio does NOT follow the picture — the highest-value line here

Rudis has **no linked-clip concept**. After `detachAudio` the picture and the
sound are two independent clips with two independent ids. Retiming one and not
the other is a SILENT desync that grows across the whole clip: at 2.0x on a
4-second clip the sound ends 2 seconds after the picture.

Always retime BOTH in the same call by listing both ids:

```json
{
  "name": "set_clip_properties",
  "input": { "clipIds": ["clip-3", "clip-3~a1"], "speed": 2.0 }
}
```

Read `get_timeline` first and find the audio-track clip that belongs to the
video clip you are retiming. If you cannot tell which audio clip pairs with
which video clip, ask — do not guess.

A video clip whose audio has NOT been detached carries its own sound, and that
sound retimes with it automatically. The trap is specifically the detached
case.

## 3. Retiming leaves a gap or an overlap — it does not ripple

Retime changes the clip's TIMELINE length while keeping its SOURCE range.
Rudis trim/speed semantics deliberately do NOT ripple neighbours:

- **Speeding up (>1.0x) leaves a GAP after the clip.** A 4s clip at 2.0x now
  occupies 2s, and there is 2s of black before the next clip.
- **Slowing down (<1.0x) OVERLAPS the next clip.** On the same track the LATER
  clip wins for the overlap, so the slowed clip's tail is hidden.

**LEAVE THE GAP. Do not close it unless the user asks.** Retiming one clip is
not permission to move a different one. The user positioned those clips; a
speed change is about the clip they named, and silently sliding everything
after it is a second edit they did not request — and one they may not notice
until much later in the timeline. This matches Premiere's Speed/Duration
(whose "Ripple Edit, Shifting Trailing Clips" is OFF by default) and Resolve's
Change Clip Speed. Say what you left behind — "that leaves a 2s gap" — rather
than fixing it unasked.

Close or open room ONLY when the user asks for it, in the same turn:

- to close a gap on request ("close the gap", "keep it tight", "no black"):
  `move_clips` each following clip earlier by the amount the retimed clip
  shrank (`old_length - new_length`), or `ripple_delete_ranges` on the gap
  range to close it in one call.
- to open room before slowing a clip on request: `move_clips` the following
  clips later by the amount it will grow.

The OVERLAP case is different and you should raise it unprompted: slowing a
clip hides its own tail behind the next clip, so the user loses footage they
asked to see. State it plainly and offer to move the later clips.

Compute the new length yourself: `new_length = source_length / speed` for a
constant speed. For a ramp, re-read `get_timeline` after the call —
`duration_us` / `duration_frames` on the clip report the real retimed length.

**Do not compute a retimed clip's duration from its source in/out points.**
Under retime `duration_frames` is NOT
`source_out_frame - source_in_frame`. Read `duration_frames`.

## 4. Ramp craft

A ramp's keys are `frame` (clip-relative, PROJECT fps) and a single-number
`value` (the rate at that moment). `interp` governs the segment from that key
to the next:

- `"hold"` — a HARD speed segment: the rate steps at the key and stays flat.
  This is the classic NLE speed blade (FCP's Blade Speed, Premiere's speed
  segments). Use it when the user asks for "this part fast, that part normal".
- `"smooth"` — an eased ramp in and out (Resolve's Retime Curve default). Use
  it when the user asks to "ramp into" or "ease into" slow motion.
- `"linear"` — a constant-rate change; usually reads as mechanical.

Speed keys are **INTEGRATED over time**, unlike every other keyframed
property. Each key is a RATE, and the clip's source position is the
accumulated area under the curve. Two consequences:

- the clip's TIMELINE LENGTH changes when you add or move speed keys;
- moving ONE key shifts the content of every LATER frame, not just its own
  neighbourhood.

Put speed points relative to the ACTION, not to round numbers. The beat you
are decorating — the impact, the landing, the reveal — should sit in the
middle of the slow section, with the ramp starting shortly before it.

Standard shapes worth knowing by name (CapCut's vocabulary, which users
borrow):

- **hero / slow-fast-slow** — normal, drop to slow over the beat, back to
  normal. Three or four keys.
- **bullet / fast-slow-fast** — fast approach, hold the moment, fast exit.
- **time-lapse** — one constant high speed, not a ramp. Use
  `set_clip_properties` with `speed`, not `set_keyframes`.

A three-key decelerate-into-slow-motion-and-back, on a 5s 30fps clip:

```json
{
  "name": "set_keyframes",
  "input": {
    "clipId": "clip-3",
    "property": "speed",
    "keyframes": [
      { "frame": 0,   "value": [1.0], "interp": "smooth" },
      { "frame": 75,  "value": [0.4], "interp": "smooth" },
      { "frame": 149, "value": [1.0], "interp": "smooth" }
    ]
  }
}
```

Pass an EMPTY `keyframes` array to remove the ramp entirely.

**A speed track is capped at 64 keys, not 1000.** `set_keyframes`' general
"at most 1000 keyframes per track" does NOT apply to `property: "speed"` — it
is the one property with a **64**-key ceiling, and a longer track is rejected
outright (the whole call fails; `set_keyframes` is all-or-none, so nothing
changes). That is not a limitation to work around: speed keys are integrated
on the per-frame render path, so every key costs real work at export time,
and a real ramp is 2-6 points. If you find yourself wanting dozens of keys,
you are hand-drawing a curve the `smooth` interpolation already draws for you.

**Ramped audio.** Audio time-stretches with its pitch preserved, which is
correct but rarely musical through a ramp: speech warbles and music loses its
tempo grid. For a ramped shot, prefer ducking the original audio under a music
bed or narration rather than letting it warp — see the `audio-ducking`
playbook. For a CONSTANT speed change within roughly 0.8x-1.25x the stretched
audio is usually fine as-is.
