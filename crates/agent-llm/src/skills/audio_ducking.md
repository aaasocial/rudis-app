## Ducking music under narration or dialogue

A duck is a LEVEL change — the music stays audible but sits clearly under the voice. It is never an on/off toggle.

1. If the music and the narration/dialogue are mixed in ONE clip's audio, call `detachAudio` FIRST — you cannot lower just one of two sources that live inside the same clip's audio. After detaching, the background/music audio is its own clip you can adjust independently.
2. THEN call `setClipVolume` with a negative `gainDb` on the background/music clip. A useful duck range is -12 to -18 dB: still present, clearly under the voice. Start at -12 dB unless the user asked for the music to be barely there.

Never use `setClipMuted` for a duck — muting removes the music entirely (that is silence, not a duck), and un-muting restores UNITY gain, NOT a remembered pre-mute custom level (v1 has no remembered-volume state), so a mute/un-mute round trip silently destroys any custom level the clip had.
