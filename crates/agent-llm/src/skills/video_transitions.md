Generating the bridge between two clips (A -> B) is an INTERPOLATION, not
a description. This playbook is the full recipe; the core both-frames rule
is also in generate_ai_video's own description.

## The recipe
- Pass BOTH endpoints as real frames: referenceSource "clipEnd" +
  referenceClipId for the clip being left, destinationSource "clipStart" +
  destinationClipId for the clip being joined. The clip ids are in the
  timeline state you were given.
- The FRAMES are what make it a transition, and they are never optional:
  the request shape follows the frames you pass, so both endpoints or it
  is not a bridge at all. In the `model` field, name a model that supports
  a first+last keyframe pair -- the rulebook's cost table marks which do,
  and the cheapest model is NOT one of them -- and the result discloses
  which model ran.
- Do NOT settle for describing the destination in the prompt text. A
  prompt-only destination makes the model invent an ending, so the
  generated shot does not land on the next clip and the cut still jumps --
  the exact failure this recipe exists to prevent.

## The prompt
- Lead with the camera MOVE whichever model you named, because with both
  frames supplied the move is the payload: camera path, speed, altitude,
  what the lens does. If you named a Veo-family model, structure the
  prompt with the five-part labels, one per line -- "Cinematography:",
  "Subject:", "Action:", "Context:", "Style & Ambiance:" -- and put
  Cinematography first; otherwise write Runway's own flowing structure
  with the move up front. Never re-describe either endpoint; they are
  already given as pixels, and repeating them in words competes with
  them.
- Name the KIND of bridge, not just that one happens: a physical camera
  move, a time-of-day shift, a morph, a match cut, or an object wipe (the
  camera passes behind or through something). Left unnamed, the model
  picks the mechanism itself, and its default when unsure is a blurry
  crossfade -- the mushy dissolve this rule exists to prevent.
- Say what must NOT change across the bridge. Faces, wardrobe, product
  colors, logos, and on-screen text drift mid-transition unless clamped --
  add a hold like "identical hairstyle, clothing, and lighting throughout"
  for whatever both endpoints share, or the bridge visibly mutates the
  subject it is supposed to carry over.
- A Canvas sketch of a camera path is PROMPT CONTEXT for that move --
  read the path and put it into the Cinematography line. Do NOT pass the
  sketch as referenceSource: that makes the drawing itself the video's
  first frame, so the clip opens on a line drawing instead of the footage.

## Before spending
- The generated clip has NO audio. Do not spend prompt words on how the
  bridge should sound, and do not tell the user it will have a whoosh or
  an SFX hit -- it will arrive silent. If the bridge needs sound, that is
  a separate generate_ai_audio call or existing media on an audio track.
- BEFORE spending a paid generation, judge whether the two endpoint frames
  can plausibly interpolate: different scenes, subjects, or perspective
  families degrade into a morph or crossfade NO prompt can rescue;
  mismatched aspect ratios force crop/pad that muddies the bridge; a
  motion-blurred last frame is read as CONTENT, so the clip opens smeary --
  prefer a settled, sharp frame. If the pair looks unviable, say so and
  askUser rather than silently spending.
