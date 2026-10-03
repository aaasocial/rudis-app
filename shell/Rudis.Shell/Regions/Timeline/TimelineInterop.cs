using System.Runtime.InteropServices;

namespace Rudis.Shell.Regions;

/// <summary>
/// The C# side of the Timeline frame contract: the six <c>#[repr(C)]</c> structs
/// mirrored field-for-field from <c>crates/timeline-render/src/frame.rs</c>, the six
/// exports of <c>rudis_timeline.dll</c>, and the six-code error table.
///
/// <para><b>Why this file lives in the WinUI-free directory.</b> It needs no UI type
/// at all: the panel arrives as an <c>nint</c> COM pointer, obtained by the region's
/// code-behind (a file BESIDE this directory) and passed straight through. Keeping
/// the declarations here means the layout canary — the thing that actually protects
/// against memory corruption — runs in a plain <c>net9.0-windows</c> test host with
/// no window and no GPU.</para>
///
/// <para><b>The layout is PINNED, on both sides, to committed literals.</b>
/// <c>crates/timeline-render/tests/layout_canary.rs</c> asserts
/// <c>size_of</c>/<c>align_of</c>/<c>offset_of</c> as numbers;
/// <c>TimelineInteropLayoutTests</c> asserts the same numbers from here, and
/// <c>.planning/phases/52-timeline-region/artifacts/52-04-renderer.md</c> §2 is the
/// transcription both read. Three traps that file records, restated where they bite:
/// </para>
/// <list type="number">
/// <item>The 4-byte padding holes at <c>RudisTimelineClip</c>+44 and +60 are ABI.
///   They exist because a pointer and an <c>i64</c> both want 8-byte alignment, and
///   closing them by reordering fields is a perfectly reasonable optimisation that
///   silently breaks the contract. The canary fails on it deliberately.</item>
/// <item><c>PeaksBlockUs</c>/<c>ClipInUs</c>/<c>ClipDurUs</c> are Rust <c>i64</c>, so
///   they are <c>long</c> here. A <c>ulong</c> mirror reads -1 as 1.8e19 and places a
///   clip past the heat death of the timeline — which RENDERS, and is therefore far
///   worse than a compile error.</item>
/// <item>Every <c>_px</c> field is PHYSICAL pixels, already multiplied through by
///   <c>Scale</c>. The hot path works exclusively in LOGICAL px and converts at
///   exactly one seam (<see cref="TimelineViewport.LogicalToPhysical"/>, D-14).</item>
/// </list>
///
/// <para><b>Colour encoding:</b> every colour is <c>0xAARRGGBB</c>, the byte order
/// .NET's colour struct and XAML's own eight-digit notation already use, so the C#
/// side packs a resolved token with no reordering. The renderer holds NO colour of
/// its own (CLAUDE.md rule 7 surviving a GPU boundary a shader could never have read
/// a XAML dictionary across) — see <see cref="TimelinePalette"/>.</para>
/// </summary>
internal static class TimelineClipFlags
{
    /// <summary>Bit 0 — the single primary selection (D-08). Draws the 2px accent
    /// outline (README:160).</summary>
    public const uint Selected = 1u << 0;

    /// <summary>Bit 1 — the clip has undetached audio (D-20:
    /// <c>has_audio &amp;&amp; !audio_detached</c>), so the waveform fill applies.
    /// Video clips included, not only clips on an audio lane.</summary>
    public const uint HasAudio = 1u << 1;

    /// <summary>
    /// Bit 2 — <b>RESERVED. NOTHING IN THIS CODEBASE MAY SET IT.</b>
    ///
    /// <para>The design handoff marks the agent-edited clip with a star and a 2px
    /// accent border (README:137). There is no domain field to drive it:
    /// <c>Clip.agentEdited</c> was reserved in
    /// <c>.planning/research/ARCHITECTURE.md:413</c> but never added to
    /// <c>crates/core::Clip</c>, and <c>crates/core</c> is under the annotated
    /// <c>engine-axis-freeze</c> tag, so adding one is not an available move either.
    /// Inventing the field would be new backend scope smuggled in under a UI port —
    /// exactly what D-06 refused for the handoff's `T` lane.
    /// <c>flag_bit2_is_never_set</c> asserts the absence over every clip variant the
    /// model can produce.</para>
    /// </summary>
    public const uint ReservedAgentEdited = 1u << 2;

    /// <summary>
    /// Bit 3 — the strip fields on <see cref="RudisTimelineClip"/> carry D-13's
    /// POSTER PLACEHOLDER rather than a real filmstrip sheet, so the renderer draws
    /// ONE stretched quad across the body instead of tiling it.
    ///
    /// <para>Set by this side when it serves the poster fallback: a media item whose
    /// extraction has not finished, has been evicted from the atlas, or will never
    /// produce frames at all (D-15 collapses all three into the same drawing).
    /// Consumed by plan 53.2-05's textured pass; plan 53.2-03 only DEFINES it.</para>
    /// </summary>
    public const uint FilmstripPlaceholder = 1u << 3;

    /// <summary>Every bit this build understands. Anything outside it is ignored by
    /// the renderer rather than treated as an error — forward compatibility for a C#
    /// side that is a build ahead of the DLL.
    ///
    /// <para>Bit 2 is still absent, deliberately, now that bit 3 sits beside it: the
    /// agent-edited reservation is a gap in the DOMAIN model, and a mask that grew
    /// once is exactly where someone would "tidy" that gap closed.</para></summary>
    public const uint KnownMask = Selected | HasAudio | FilmstripPlaceholder;
}

/// <summary>
/// Every colour the Timeline can draw, resolved from <c>Theme/Tokens.xaml</c> BY NAME
/// and uploaded once by <c>rudis_timeline_set_palette</c>.
///
/// <para>20 contiguous <c>u32</c>, no padding: 80 bytes, 4-byte aligned. Field names
/// are the design handoff's own token names, so "did the renderer use the right
/// token?" is a question anyone can answer by reading two files side by side.</para>
/// </summary>
[StructLayout(LayoutKind.Sequential)]
internal unsafe struct RudisTimelinePalette
{
    /// <summary><c>bg-bar</c> — the ruler band and the sticky gutter.</summary>
    public uint BgBar;

    /// <summary><c>bg-app</c> — the base lane band, and the renderer's clear colour.</summary>
    public uint BgApp;

    /// <summary><c>bg-panel</c> — the alternating (audio) lane band.</summary>
    public uint BgPanel;

    /// <summary><c>border-subtle</c> — Timeline track separators (README:197).</summary>
    public uint BorderSubtle;

    /// <summary><c>border-hairline</c> — minor ruler graduations.</summary>
    public uint BorderHairline;

    /// <summary><c>text-primary</c> — carried so the palette is one complete object
    /// rather than two partial ones that can disagree; the live timecode itself is
    /// drawn by the XAML toolbar, not by the renderer.</summary>
    public uint TextPrimary;

    /// <summary><c>text-secondary</c> — lane labels in the gutter.</summary>
    public uint TextSecondary;

    /// <summary><c>text-faint</c> — the region tag and ruler tick labels.</summary>
    public uint TextFaint;

    /// <summary><c>accent</c> — the playhead and the selection outline.</summary>
    public uint Accent;

    /// <summary><c>accent-bright</c> — trim handles and snap guides.</summary>
    public uint AccentBright;

    /// <summary><c>clip-audio-fill</c> — the handoff's mint audio-clip fill.</summary>
    public uint ClipAudioFill;

    /// <summary><c>clip-waveform-line</c> — the translucent dark green beside the
    /// mint fill. Consumed by the renderer's waveform module, which plan 52-08
    /// fills.</summary>
    public uint ClipWaveformLine;

    /// <summary>The 8-entry clip poster palette, assigned per media item and cycled
    /// (README:213). A fixed buffer rather than a marshalled array so the struct stays
    /// blittable and its layout is exactly the Rust one.</summary>
    public fixed uint ClipPoster[8];
}

/// <summary>One track lane: a horizontal band with a label in the gutter. 32 bytes.</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct RudisTimelineLane
{
    /// <summary>Top edge, PHYSICAL px from the surface top.</summary>
    public float YPx;

    /// <summary>Band height, PHYSICAL px (48 / 42 logical, README:239).</summary>
    public float HPx;

    /// <summary><c>0</c> = video, <c>1</c> = audio. There is deliberately no <c>2</c>
    /// for the handoff's `T` lane: the domain has exactly two track kinds (D-06).</summary>
    public uint Kind;

    /// <summary>UTF-8 label bytes (<c>V1</c>, <c>A1</c>, …). Borrowed for one call.</summary>
    public nint LabelPtr;

    /// <summary>Length of <see cref="LabelPtr"/> in bytes.</summary>
    public uint LabelLen;
}

/// <summary>One ruler graduation. 24 bytes.</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct RudisTimelineTick
{
    /// <summary>PHYSICAL px from the surface left (already past the gutter).</summary>
    public float XPx;

    /// <summary>Non-zero for a major (labelled, full-height) graduation.</summary>
    public uint Major;

    /// <summary>UTF-8 timecode label, e.g. <c>0:05</c>. May be null.</summary>
    public nint LabelPtr;

    /// <summary>Length of <see cref="LabelPtr"/> in bytes.</summary>
    public uint LabelLen;
}

/// <summary>
/// One clip: a rectangle, two colours, a few flags, a borrowed label, and (from plan
/// 53.2-03) the band colour plus the filmstrip strip's pointer and grid. 144 bytes.
///
/// <para>This is the struct the whole phase is about. At 1,000 clips the C# side
/// hands over one contiguous array of these and the renderer walks it — no object per
/// clip on either side of the boundary, which is SHELL-05 expressed as a type.</para>
///
/// <para><b>The strip fields are here rather than behind a new export, and that is
/// D-12's own CORRECTION.</b> The decision was originally written as "C# uploads into
/// a bounded wgpu texture atlas" — not buildable as stated, because no
/// <c>wgpu::Device</c> is reachable from C# anywhere in this codebase; every wgpu
/// object lives inside one of the two Rust cdylibs. The substance is unchanged: bytes
/// cross the ABI once, read-only, and residency is a bounded LRU. What moved is only
/// which side owns which half — <b>this side decides WHICH strips stay resident</b>
/// (the <see cref="PeakCache"/> policy shape it already owns) and
/// <b><c>crates/timeline-render</c> owns the texture and the upload</b>, receiving
/// pointer+length through this struct, exactly as <see cref="PeaksPtr"/> already
/// proves. <c>rudis_timeline.dll</c>'s export surface is pinned at SIX and
/// test-enforced; a seventh would have tripped that gate by design.</para>
/// </summary>
[StructLayout(LayoutKind.Sequential)]
internal struct RudisTimelineClip
{
    /// <summary>Left edge, PHYSICAL px from the surface left.</summary>
    public float XPx;

    /// <summary>Top edge, PHYSICAL px from the surface top.</summary>
    public float YPx;

    /// <summary>Width, PHYSICAL px. Non-finite or non-positive means the quad is
    /// SKIPPED by the renderer, not clamped to something plausible-looking.</summary>
    public float WPx;

    /// <summary>Height, PHYSICAL px.</summary>
    public float HPx;

    /// <summary>Body fill, <c>0xAARRGGBB</c>, resolved from a named token.</summary>
    public uint Fill;

    /// <summary>Border, <c>0xAARRGGBB</c> — the fill darkened (README:213). The
    /// darkening is computed here so the renderer never invents a colour.</summary>
    public uint Border;

    /// <summary>See <see cref="TimelineClipFlags"/>.</summary>
    public uint Flags;

    /// <summary>Trim-handle width in PHYSICAL px, per edge. It is the hit tester's own
    /// zone width (<see cref="TimelineHitTester.HandleWidthFor"/>) converted at the one
    /// boundary, so the DRAWN handle and the GRABBABLE zone can never disagree.</summary>
    public float TrimHandlePx;

    /// <summary>UTF-8 clip label. Borrowed for one call.</summary>
    public nint LabelPtr;

    /// <summary>Length of <see cref="LabelPtr"/> in bytes.</summary>
    public uint LabelLen;

    // 4 bytes of PADDING here (offset 44) — the pointer below wants 8-byte alignment.
    // It is ABI. See this file's own remarks.

    /// <summary><c>u8</c> RMS peaks from <c>crates/waveform</c>'s cache. NULL until
    /// plan 52-08 wires the read path; the renderer already accepts it and draws
    /// nothing, so 52-08 adds no ABI change and no new pipeline.</summary>
    public nint PeaksPtr;

    /// <summary>Length of <see cref="PeaksPtr"/> in bytes (one byte per block).</summary>
    public uint PeaksLen;

    // 4 bytes of PADDING here (offset 60), before the three signed 64-bit fields.

    /// <summary>Microseconds of source audio each peak byte summarises. Read from the
    /// cache entry rather than assumed, so a cache-version bump cannot silently
    /// mis-scale old data. <b>Signed</b> — see this file's remarks.</summary>
    public long PeaksBlockUs;

    /// <summary>The clip's in-point within its source media, µs. <b>Signed.</b></summary>
    public long ClipInUs;

    /// <summary>The clip's duration, µs. <b>Signed.</b></summary>
    public long ClipDurUs;

    // ────────────────────────────────────────────────────────────────────────
    // APPENDED by plan 53.2-03, at the END, in the SAME COMMIT as the Rust side
    // and both layout canaries. Every offset above is byte-identical to what
    // 52-04 committed; only the total size moved, 88 -> 144.
    //
    // Inserting any of these anywhere else would have shifted PeaksPtr and the
    // three signed µs fields under a Rust side that reads them BY OFFSET — a
    // silent corruption rather than a failed test. Anything a future plan adds
    // goes at the end too.
    // ────────────────────────────────────────────────────────────────────────

    /// <summary>Band fill, <c>0xAARRGGBB</c>, resolved from a NAMED token like every
    /// other colour that crosses this boundary (convention 7). <c>0</c> means "fall
    /// back to <see cref="Fill"/>" — a band that merges with the body until the token
    /// is wired, rather than a hole where a colour should be.</summary>
    public uint BandFill;

    /// <summary>Strip sheet byte length. <c>0</c> means no strip data, and the body
    /// renders <see cref="Fill"/>. D-04's degraded state and the not-yet-extracted
    /// state are deliberately the SAME drawing (D-15: the caller cannot distinguish
    /// "miss", "not yet" and "never").</summary>
    public uint StripLen;

    /// <summary>Tile cell width in the sheet, px. The renderer validates it in
    /// <c>[1, 512]</c> before any use — these numbers describe a buffer's geometry,
    /// so they are untrusted input on the far side however carefully written here.</summary>
    public uint StripTileW;

    /// <summary>Tile cell height, px. Validated in <c>[1, 512]</c>.</summary>
    public uint StripTileH;

    /// <summary>Tiles per sheet row. Validated in <c>[1, 64]</c>.</summary>
    public uint StripTilesPerRow;

    /// <summary>Total tiles the finished strip will hold. Validated in
    /// <c>[1, 4096]</c>.</summary>
    public uint StripTotalTiles;

    /// <summary>Tiles valid so far (D-14's progressive fill). Must be
    /// <c>&lt;= StripTotalTiles</c> or the renderer ignores the strip entirely. Doubles
    /// as the residency REVISION: growth means the resident texture is stale and must
    /// be re-uploaded.</summary>
    public uint StripCompletedTiles;

    /// <summary>Explicit padding, always <c>0</c>. It is the eighth of eight
    /// <c>uint</c>s and its job is to land <see cref="StripPtr"/> on an already
    /// 8-aligned offset 120, so the append introduces NO implicit hole. Deleting it
    /// as "unused" would move the pointer and break the contract silently.</summary>
    public uint StripPad;

    /// <summary>The sheet bytes: raw RGBA rows,
    /// <c>sheetWidth = StripTilesPerRow * StripTileW</c>. Borrowed for ONE render call
    /// only — pin it for the duration exactly as <see cref="PeaksPtr"/> is pinned, and
    /// never hand over a buffer this side may move.</summary>
    public nint StripPtr;

    /// <summary>Source microseconds per tile (D-06's one fixed extraction density —
    /// zoom selects client-side and costs no decode). <c>0</c> for placeholders.
    /// <b>Signed</b>, like the three µs fields above and for the same reason.</summary>
    public long StripIntervalUs;

    /// <summary>Residency key: FNV-1a 64 of the media id, computed on this side
    /// (placeholders use the same hash with bit 0 flipped). Keyed per MEDIA, never per
    /// clip — D-07: a trim, a split and a duplicate all share one media's one strip,
    /// which is the entire reason those edits cost zero new decode.
    /// <b>Unsigned</b> — it is a hash, not a measurement.</summary>
    public ulong StripKey;
}

/// <summary>One whole Timeline frame: five pointer/length pairs and a handful of
/// scalars. 112 bytes.</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct RudisTimelineFrame
{
    /// <summary>Surface width in PHYSICAL px (must match the last attach/resize).</summary>
    public uint SurfaceWPx;

    /// <summary>Surface height in PHYSICAL px.</summary>
    public uint SurfaceHPx;

    /// <summary>Display scale (1.25 at 125%). The geometry above is ALREADY scaled;
    /// this rides along because the text pass rasterises at physical size, and a label
    /// shaped for the wrong scale is blurry rather than wrong — the failure mode nobody
    /// notices for a month.</summary>
    public float Scale;

    /// <summary>Gutter width, PHYSICAL px (46 logical, README:239).</summary>
    public float GutterWPx;

    /// <summary>How much vertical space at the top of the surface is NOT the
    /// renderer's, PHYSICAL px — the `Timeline › Toolbar` band (36 logical), which is
    /// a real XAML control drawn over this surface.</summary>
    public float HeaderHPx;

    /// <summary>Ruler height, PHYSICAL px (20 logical, README:239).</summary>
    public float RulerHPx;

    /// <summary>Lane array. May be null iff <see cref="LanesLen"/> is 0.</summary>
    public nint LanesPtr;

    /// <summary>Lane count. Bounded by the renderer at 256.</summary>
    public uint LanesLen;

    /// <summary>Clip array — already viewport-culled, so O(visible), not O(project).</summary>
    public nint ClipsPtr;

    /// <summary>Clip count. Bounded by the renderer at 100,000.</summary>
    public uint ClipsLen;

    /// <summary>Ruler tick array.</summary>
    public nint TicksPtr;

    /// <summary>Tick count. Bounded by the renderer at 4,096.</summary>
    public uint TicksLen;

    /// <summary>Playhead x, PHYSICAL px from the surface left. Non-finite draws no
    /// playhead at all rather than a plausible-looking line at zero.</summary>
    public float PlayheadXPx;

    /// <summary>Drag/trim preview rectangles. Plan 52-07's; null/0 until then.</summary>
    public nint GhostPtr;

    /// <summary>Ghost count. Bounded by the renderer at 8.</summary>
    public uint GhostLen;

    /// <summary>Snap-guide x positions. Plan 52-07's; null/0 until then.</summary>
    public nint SnapGuidesPtr;

    /// <summary>Snap-guide count. Bounded by the renderer at 64.</summary>
    public uint SnapGuidesLen;

    /// <summary>
    /// <c>0</c> means "nothing changed since the last render". The renderer returns
    /// BEFORE acquiring a surface texture and increments its
    /// <c>skipped_clean_frames</c> counter, so an idle Timeline costs a comparison and
    /// nothing else — no GPU work, no present, no vsync wait.
    ///
    /// <para>The caller still CALLS render with a clean frame, deliberately: the
    /// counter climbing while <c>frames_rendered</c> stays flat is the evidence for
    /// "an idle Timeline does no GPU work", and a caller that simply skipped the call
    /// would have no such evidence.</para>
    /// </summary>
    public uint Dirty;

    // ── APPENDED by plan 53.2-03, same commit, same discipline as the clip struct. ──

    /// <summary>
    /// D-01's title-band height, PHYSICAL px — 14 LOGICAL px multiplied through by
    /// <see cref="Scale"/> on this side, like every other <c>_px</c> field here.
    ///
    /// <para><c>&lt;= 0</c> means NO BAND ANYWHERE, and the renderer then produces
    /// exactly the pre-phase picture: the same quad count and the same label rect.
    /// That is the back-compat clause, not leniency — a build that sends a zeroed
    /// tail must get the old drawing rather than a subtly different one.</para>
    ///
    /// <para>The renderer clamps it PER CLIP to that clip's own height, so a lane
    /// shorter than the band is all band rather than a body with negative height
    /// (D-04: the band always draws, the frame body degrades first).</para>
    /// </summary>
    public float BandHPx;

    /// <summary>Explicit padding, always <c>0</c>. Without it the struct would carry
    /// an IMPLICIT 4-byte trailing hole to reach its 8-byte alignment, and the whole
    /// discipline on both sides of this contract is that padding is written down
    /// rather than inferred.</summary>
    public uint BandPad;
}

/// <summary>Counters read back through <c>rudis_timeline_stats</c>. 64 bytes.
/// Monotonic for the life of one attached renderer; a detach/attach cycle starts
/// fresh.</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct RudisTimelineStats
{
    /// <summary>Wall time of the most recent render that actually presented, µs.</summary>
    public ulong LastRenderUs;

    /// <summary>Frames that reached <c>present()</c>.</summary>
    public ulong FramesRendered;

    /// <summary>Frames refused at the door because <c>dirty == 0</c>.</summary>
    public ulong SkippedCleanFrames;

    /// <summary>Quads emitted by the most recent rendered frame, AFTER non-finite
    /// geometry is filtered — so a skipped quad shows up as a smaller number rather
    /// than invisibly.</summary>
    public uint QuadsDrawn;

    /// <summary>Glyphs emitted by the most recent rendered frame.</summary>
    public uint GlyphsDrawn;

    /// <summary>Cumulative recoverable present failures (timeout / validation).</summary>
    public uint PresentErrors;

    /// <summary>Cumulative lost-or-outdated surface events. Treated as a recoverable
    /// reattach, never an unhandled exception (T-52-19).</summary>
    public uint DeviceLost;

    // ── APPENDED by plan 52-08, matching frame.rs's own note ──
    //
    // The two fields below were added at the END of the struct in both languages, in
    // the SAME commit, precisely so every offset above stays byte-identical to what
    // 52-04 committed and 52-06 mirrored. The only literal that moved is the total
    // size, 40 -> 48, and both canaries assert it.

    /// <summary>Waveform bars in the most recent rendered frame — the subset of
    /// <see cref="QuadsDrawn"/> the audio fill contributed. Separate because "the
    /// waveform drew nothing" and "the frame drew nothing" are different problems.</summary>
    public uint WaveformQuadsDrawn;

    /// <summary>Clips whose fill was refused because the renderer's per-frame waveform
    /// quad budget was exhausted (T-52-36).</summary>
    public uint WaveformTruncatedClips;

    // ── APPENDED by plan 53.2-05, matching frame.rs's own note ──
    //
    // Four more at the END, in the SAME commit on both sides, so every offset above
    // stays byte-identical. The only literal that moved is the total size, 48 -> 64,
    // and both canaries assert it. Four is an EVEN count deliberately: the ten uints
    // pair up exactly on the struct's 8-byte alignment, so there is no trailing hole.

    /// <summary>Filmstrip tiles in the most recent rendered frame. NOT a subset of
    /// <see cref="QuadsDrawn"/>, unlike the waveform pair above — tiles come from a
    /// second, textured pipeline, so they are counted separately because they genuinely
    /// are separate.</summary>
    public uint FilmstripQuadsDrawn;

    /// <summary>Clips whose tiles were refused because the renderer's per-frame
    /// filmstrip quad budget was exhausted (T-53.2-20). Deliberately NOT incremented
    /// for a clip whose strip merely was not atlas-resident: those are different
    /// problems with different fixes, and the atlas's side is the two counters
    /// below.</summary>
    public uint FilmstripTruncatedClips;

    /// <summary>Strips resident in the renderer's filmstrip atlas right now — a LEVEL,
    /// not a per-frame count.</summary>
    public uint AtlasResidentStrips;

    /// <summary>Cumulative atlas evictions since attach — a TOTAL, not a per-frame
    /// count, because "the atlas is thrashing" is a question about a trend.</summary>
    public uint AtlasEvictions;
}

/// <summary>
/// The renderer's six-code error contract. The source of truth is
/// <c>.planning/phases/52-timeline-region/artifacts/52-04-renderer.md</c> §3, and
/// <c>error_codes_are_declared_exactly_once_and_match_the_rust_table</c> pins this
/// enum to it.
///
/// <para>Never a bool: the difference between a lost surface and a malformed frame is
/// the difference between reattaching and fixing a bug.</para>
/// </summary>
internal enum TimelineStatus
{
    /// <summary>Succeeded — <b>including a deliberately skipped clean frame.</b></summary>
    Ok = 0,

    /// <summary>The handle was null. A caller lifetime bug; log it.</summary>
    NullHandle = -1,

    /// <summary>A null pointer with a non-zero length, or a length past its array's
    /// bound. A frame-builder bug; log it and do not retry the same frame.</summary>
    BadBuffer = -2,

    /// <summary>Swapchain lost or outdated. <b>Detach and re-attach</b> — recoverable,
    /// never an unhandled exception (T-52-19). 52-04 §6 proved the re-attach path on
    /// real hardware rather than assuming it.</summary>
    SurfaceLost = -3,

    /// <summary>A Rust panic was contained at the boundary. The renderer remains
    /// usable; log it with the stats.</summary>
    PanicCaught = -4,

    /// <summary>Attach or resize was attempted off the panel's own thread. Dispatch to
    /// the panel's UI thread and retry.</summary>
    WrongThread = -5,
}

/// <summary>
/// A managed <see cref="SafeHandle"/> over the opaque <c>RudisTimeline*</c>, the same
/// discipline <c>RudisCtxHandle</c> applies to the engine context: the runtime
/// guarantees <see cref="ReleaseHandle"/> runs AT MOST ONCE, so a missed detach cannot
/// leak a GPU device and a double detach cannot be a double free (T-52-27).
///
/// <para><b>Release is expected to run on the panel's UI thread</b>, from the region's
/// window-closing teardown. <c>rudis_timeline_detach</c> performs the whole four-step
/// unbind sequence internally (join → unbind → drop → release), and its middle step
/// reaches a COM call with thread affinity. The finalizer path is a LEAK BACKSTOP, not
/// the intended route: it would run off the UI thread, where that COM call refuses and
/// the device is merely dropped. That is strictly better than leaking a device, and it
/// is why the region disposes explicitly rather than relying on this.</para>
/// </summary>
internal sealed class TimelineHandle : SafeHandle
{
    public TimelineHandle() : base(nint.Zero, ownsHandle: true) { }

    /// <summary>Null means attach failed — the return type is a pointer, so null is
    /// the only failure signal it can carry. Callers null-check, never assume.</summary>
    public override bool IsInvalid => handle == nint.Zero;

    protected override bool ReleaseHandle() =>
        TimelineNative.rudis_timeline_detach(handle) == TimelineStatus.Ok;
}

/// <summary>
/// P/Invoke surface for <c>rudis_timeline.dll</c> — the SECOND native artifact beside
/// <c>rudis_ffi.dll</c>, staged by <c>crates/timeline-render/Rudis.Timeline.targets</c>.
///
/// <para><b>Why this does not go through <c>RudisNative</c>'s serialized worker.</b>
/// That worker exists to serialize the ENGINE ABI: one library, one context, one
/// logical flow, with a measured ordering hazard (T-50-12) behind it.
/// <c>rudis_timeline.dll</c> is a different library with no shared state, and its
/// thread-affine calls — attach and resize — must run on the panel's own UI thread by
/// construction, because the COM call they reach refuses every other thread. Posting
/// them to a background worker would BE the bug.</para>
///
/// <para><b>Source-generated marshalling only</b> (v7-STACK Q3): the legacy
/// runtime-marshalled attribute is banned in this codebase.</para>
/// </summary>
internal static partial class TimelineNative
{
    private const string Library = "rudis_timeline";

    /// <summary>Bind an independent DX12 device to the panel and return an opaque
    /// handle, or an invalid one on any failure. <b>UI thread.</b> The pointer is an
    /// <c>IInspectable*</c>; the Rust side does its own QueryInterface, so a wrong
    /// pointer is a named failure rather than undefined behaviour.</summary>
    [LibraryImport(Library)]
    internal static partial TimelineHandle rudis_timeline_attach(
        nint panel, uint widthPx, uint heightPx, float scale);

    /// <summary>Reconfigure for a new surface size, in PHYSICAL px. <b>UI thread</b> —
    /// reconfiguring reaches the same thread-affine COM call attach does.</summary>
    [LibraryImport(Library)]
    internal static partial TimelineStatus rudis_timeline_resize(
        TimelineHandle handle, uint widthPx, uint heightPx, float scale);

    /// <summary>Upload the palette resolved from <c>Theme/Tokens.xaml</c>. <b>The
    /// renderer draws NOTHING until this has been called once</b> — deliberately, since
    /// it has no colour of its own.</summary>
    [LibraryImport(Library)]
    internal static partial TimelineStatus rudis_timeline_set_palette(
        TimelineHandle handle, in RudisTimelinePalette palette);

    /// <summary>Render one frame from one flat description. The dirty gate runs first,
    /// before the arrays are even looked at.</summary>
    [LibraryImport(Library)]
    internal static partial TimelineStatus rudis_timeline_render(
        TimelineHandle handle, in RudisTimelineFrame frame);

    /// <summary>Copy the counters out.</summary>
    [LibraryImport(Library)]
    internal static partial TimelineStatus rudis_timeline_stats(
        TimelineHandle handle, out RudisTimelineStats stats);

    /// <summary>Destroy the handle and unbind the panel. Reachable ONLY from
    /// <see cref="TimelineHandle.ReleaseHandle"/> — the raw pointer never leaves the
    /// SafeHandle (the T-47-04 discipline, reapplied).</summary>
    [LibraryImport(Library)]
    internal static partial TimelineStatus rudis_timeline_detach(nint handle);
}
