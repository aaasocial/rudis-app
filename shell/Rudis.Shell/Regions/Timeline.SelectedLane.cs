using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Automation.Peers;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;

namespace Rudis.Shell.Regions;

/// <summary>
/// The SELECTED LANE half of the Timeline region — quick 260731-k9b.
///
/// <para><b>The defect this closes.</b> Owner, 2026-07-31, during the
/// <c>TIMELINE-52</c> UAT re-run: <i>"i can remove tracks that have clips that i can
/// select, however, i cannot remove empty tracks bc i have no way of selecting
/// it"</i>. Plan 52-14 mapped the add/remove-track gesture onto the toolbar and read
/// the SELECTED CLIP's track, because the drawn gutter has no per-lane control — that
/// absence IS SHELL-05. A track with no clips therefore had nothing that could name
/// it, and *Add video track* creates exactly such a track at index 0. The only escape
/// was <c>Ctrl+Z</c>. Clicking the `TrackHeader` gutter now selects the LANE, so an
/// empty track has a subject like any other.</para>
///
/// <para><b>Why a XAML overlay and not a renderer quad.</b> The obvious place for a
/// selected-lane indication is the frame builder, and it cannot go there. The
/// renderer draws <b>the gutter LAST</b>, filling the whole 46px column with
/// <c>bg-bar</c> over everything that scrolled beneath it
/// (<c>crates/timeline-render/src/quads.rs</c> step 11 — sticky-horizontally, by
/// design), and <c>RudisTimelineLane</c> carries no selected flag. Marking the gutter
/// from the renderer would therefore mean an ABI field plus a new draw step in
/// <c>crates/</c> — and <b>this task touches no Rust</b>: Phase 52 holds a verified
/// 0-commits/0-files record into those crates and <c>git status --porcelain --
/// crates/</c> must stay empty. So the indication is a single XAML element over the
/// swap-chain panel, which is the pattern this region already uses for
/// <c>DragReadout</c> (and 53.1 for its drop ghost).</para>
///
/// <para><b>It also costs the hot path nothing</b>, which the quad route would not
/// have: <see cref="TimelineFrameBuilder"/> is not touched at all here, so 52-06's
/// pinned 0 managed bytes over 10,000 warmed frame builds and 52-03's 0-byte
/// cull/hit-test gate are untouched rather than re-argued. The per-tick work below is
/// a compare and, when a lane IS selected, a walk of at most <c>Lanes.Count</c>
/// entries — with every visual-tree write change-gated, the 50-06 rule.</para>
///
/// <para><b>ONE element, not one per lane.</b> A per-lane control is what SHELL-05
/// forbids and what <c>MechanicalGatesTests</c> greps for; this is a single Border
/// that MOVES, exactly as <c>DragReadout</c> follows the drag ghost.</para>
/// </summary>
public sealed partial class Timeline
{
    /// <summary>The indicator's UIA id. A const rather than a literal at both ends so
    /// the region and its UAT cannot drift apart silently.</summary>
    internal const string SelectedLaneIndicatorAutomationId = "Timeline.SelectedLaneIndicator";

    /// <summary>The 2px `accent` outline over the selected lane's gutter cell. Created
    /// once, moved thereafter; <see langword="null"/> only if construction failed.</summary>
    private Border? _selectedLaneIndicator;

    private bool _laneIndicatorVisible;
    private double _lastLaneIndicatorTop = double.NaN;
    private double _lastLaneIndicatorHeight = double.NaN;

    /// <summary>Mirrors <c>RemoveTrackButton.IsEnabled</c>. Its own gate rather than a
    /// ride on the clip selection's, because it now follows EITHER selection and the
    /// two move independently.</summary>
    private bool _removeTrackEnabled;

    /// <summary>
    /// The selected TRACK index as of the last render tick, for 52-09's introspection
    /// hook. <c>-1</c> is "no lane is selected" and is a real published value.
    ///
    /// <para>A static beside <c>LastWaveformDiagnostics</c> rather than a field on
    /// <c>IntrospectionSnapshot</c>, for the mundane reason that the snapshot's own
    /// class is declared in <c>Timeline.xaml.cs</c> and a concurrent session holds
    /// that file. <c>volatile</c> because it is written on the UI thread and read on
    /// the hook's pipe thread; an <c>int</c> cannot tear, so one ordered store is the
    /// whole synchronisation this needs.</para>
    ///
    /// <para>Why publish it at all: a UAT that could only see the lane selection as
    /// PIXELS would be asserting on a screenshot. This is the machine-readable half,
    /// and <c>an_empty_track_can_be_selected_by_its_gutter_and_removed</c> reads it at
    /// every step.</para>
    /// </summary>
    internal static volatile int LastSelectedTrackIndex = TimelineModel.NoTrack;

    /// <summary>
    /// Build the indicator and put it over the surface, in the lane band's grid row.
    ///
    /// <para>Declared here rather than in <c>Timeline.xaml</c> deliberately: this
    /// element is meaningless without the code below that positions it, and keeping
    /// the pair in one file is the same reason the region wires its own event handlers
    /// in the constructor instead of in markup.</para>
    ///
    /// <para><c>IsHitTestVisible = false</c> is load-bearing, not tidiness. It sits
    /// directly over the gutter it marks, and a hit-testable overlay would swallow the
    /// very <c>PointerPressed</c> that selects a lane — the second click on an already
    /// selected lane would go nowhere, which reads as "selection is stuck".</para>
    /// </summary>
    private void InstallSelectedLaneIndicator()
    {
        var indicator = new Border
        {
            // README.md:160 (§ Focus & selection) — "2px `accent` outline" is the
            // handoff's selection idiom, and it is the SAME one the renderer draws
            // around a selected clip (quads.rs step 4). One idea, one look.
            BorderBrush = ResolveTokenBrush("accent"),
            BorderThickness = new Thickness(TimelineMetrics.SelectionOutlineWidth),

            // `radius-clip` — the token the renderer's own selection outline uses
            // (quads.rs `radius_clip`), taken from Theme/Tokens.xaml BY NAME rather
            // than restated as a 2 here (CLAUDE.md convention 7).
            CornerRadius = ResolveTokenRadius("radius-clip"),

            // The gutter's full width. The band is the lane's own header cell, so the
            // mark reads as "this LANE", never as "this part of the timeline".
            Width = TimelineMetrics.TrackHeaderGutterWidth,

            HorizontalAlignment = HorizontalAlignment.Left,
            VerticalAlignment = VerticalAlignment.Top,
            IsHitTestVisible = false,
            Visibility = Visibility.Collapsed,
        };

        Grid.SetRow(indicator, 1);
        AutomationProperties.SetAutomationId(indicator, SelectedLaneIndicatorAutomationId);
        AutomationProperties.SetName(indicator, "Selected track");
        AutomationProperties.SetAccessibilityView(indicator, AccessibilityView.Content);

        TimelineRoot.Children.Add(indicator);
        _selectedLaneIndicator = indicator;
    }

    /// <summary>
    /// Park the indicator over the selected lane's gutter cell, or hide it.
    ///
    /// <para>Called from the composition tick beside <c>RenderDragReadout</c>, and
    /// change-gated the same way and for the same reason: a <c>Margin</c> write forces
    /// a layout pass and a <c>Visibility</c> write raises a UIA property change, so
    /// neither happens on a tick where nothing moved. An idle Timeline with a lane
    /// selected touches the visual tree zero times.</para>
    /// </summary>
    private void RenderSelectedLaneIndicator()
    {
        // Published FIRST, so the machine-readable answer does not depend on the
        // element having been built — a UAT that could not read the selection when
        // the indicator failed to construct would report the wrong bug.
        LastSelectedTrackIndex = _model.SelectedTrackIndex;

        var indicator = _selectedLaneIndicator;
        if (indicator is null)
        {
            return;
        }

        var laneIndex = SelectedLaneIndex();
        if (laneIndex < 0 || !_viewport.IsLaneVisible(laneIndex))
        {
            HideSelectedLaneIndicator();
            return;
        }

        // The lane's y is measured from the SURFACE top, which spans BOTH grid rows;
        // this element lives in row 1, so the toolbar band comes off once, here — the
        // same subtraction RenderDragReadout makes, for the same reason.
        var top = TimelineMetrics.RulerHeight + _viewport.LaneTopPx(laneIndex) - _viewport.ScrollYPx;
        var bottom = top + _viewport.LaneHeightPx(laneIndex);

        // CLAMPED against the sticky ruler above and the region's own bottom edge
        // below. The renderer draws both bands and a XAML overlay sits OVER them, so
        // an unclamped outline on a half-scrolled lane would paint across the ruler —
        // a Grid does not clip its children, so nothing else would stop it.
        if (top < TimelineMetrics.RulerHeight)
        {
            top = TimelineMetrics.RulerHeight;
        }

        var limit = _viewport.ViewportHeightPx - TimelineMetrics.TimelineHeaderHeight;
        if (bottom > limit)
        {
            bottom = limit;
        }

        var height = bottom - top;
        if (height < 1)
        {
            HideSelectedLaneIndicator();
            return;
        }

        if (top != _lastLaneIndicatorTop || height != _lastLaneIndicatorHeight)
        {
            _lastLaneIndicatorTop = top;
            _lastLaneIndicatorHeight = height;
            indicator.Margin = new Thickness(0, top, 0, 0);
            indicator.Height = height;
        }

        if (!_laneIndicatorVisible)
        {
            _laneIndicatorVisible = true;
            indicator.Visibility = Visibility.Visible;
        }
    }

    private void HideSelectedLaneIndicator()
    {
        if (!_laneIndicatorVisible)
        {
            return;
        }

        _laneIndicatorVisible = false;
        if (_selectedLaneIndicator is { } indicator)
        {
            indicator.Visibility = Visibility.Collapsed;
        }
    }

    /// <summary>The DRAWN position of the selected track, or <c>-1</c>. The model holds
    /// a TRACK index (the command's unit); the geometry above needs a LANE index, and
    /// the two differ whenever a kind this build cannot draw was skipped (D-06 /
    /// T-52-12).</summary>
    private int SelectedLaneIndex()
    {
        var trackIndex = _model.SelectedTrackIndex;
        if (trackIndex < 0)
        {
            return -1;
        }

        var lanes = _model.Lanes;
        for (var i = 0; i < lanes.Count; i++)
        {
            if (lanes[i].TrackIndex == trackIndex)
            {
                return i;
            }
        }

        return -1;
    }

    /// <summary>
    /// The track the remove gesture would act on: the SELECTED LANE when there is one,
    /// falling back to the selected clip's track when there is not.
    ///
    /// <para><b>The fallback is the point.</b> 52-14's route — remove the selected
    /// clip's track — is preserved rather than replaced, so nothing a user already
    /// knows how to do stops working; the lane selection ADDS the subject that route
    /// could never produce.</para>
    ///
    /// <para>Returns <see langword="false"/> — never throws — when neither selection
    /// exists or the lane one is a mirror patch stale, exactly as
    /// <c>TryResolveSelectedTrack</c> does for its own.</para>
    /// </summary>
    private bool TryResolveTargetTrack(out int trackIndex, out string kind, out int clipCount)
    {
        if (!_model.HasSelectedTrack)
        {
            return TryResolveSelectedTrack(out trackIndex, out kind, out clipCount);
        }

        trackIndex = -1;
        kind = string.Empty;
        clipCount = 0;

        var laneIndex = SelectedLaneIndex();
        if (laneIndex < 0)
        {
            return false;
        }

        var lane = _model.Lanes[laneIndex];
        trackIndex = lane.TrackIndex;
        kind = lane.Kind;

        // Counted rather than cached, TryResolveSelectedTrack's rule: this runs once
        // per gesture, and a stored per-lane count would be one more thing to keep in
        // step with every mirror patch. For the case this whole task is about the
        // answer is 0, and 0 is a real answer — v6 removes such a track with no prompt
        // (main.ts `removeTrackAt` gates the confirm behind `clips.length > 0`).
        var clips = _model.Clips;
        for (var i = 0; i < clips.Count; i++)
        {
            if (clips[i].LaneIndex == laneIndex)
            {
                clipCount++;
            }
        }

        return true;
    }

    /// <summary>
    /// The remove tile follows EITHER selection.
    ///
    /// <para>52-14's comment at the old call site said a greyed tile with nothing
    /// selected is correct "because the only thing that can name a track is the
    /// selection". Still true — there is simply one more thing a selection can be. The
    /// tile is dead only when NEITHER a lane NOR a clip is selected, which is what
    /// <c>remove_track_is_disabled_with_no_selection</c> now asserts.</para>
    /// </summary>
    private void UpdateRemoveTrackAvailability(bool hasClipSelection)
    {
        var canRemove = hasClipSelection || _interaction.HasTrackSelection;
        if (canRemove == _removeTrackEnabled)
        {
            return;
        }

        _removeTrackEnabled = canRemove;
        RemoveTrackButton.IsEnabled = canRemove;
    }

    /// <summary>
    /// One named token to its <see cref="Brush"/>. Theme/Tokens.xaml keys every colour
    /// twice — <c>{token}-color</c> and a <c>{token}</c> brush (plan 50-03's
    /// convention) — and this takes the brush half.
    ///
    /// <para>A missing key is FATAL and says which key, <c>ResolveTokenColour</c>'s
    /// rule: CLAUDE.md convention 7 forbids raw hex, so the alternative to throwing is
    /// an element with no colour at all, which looks like a feature that was never
    /// built rather than a token that was misspelt.</para>
    /// </summary>
    private static Brush ResolveTokenBrush(string token) => ResolveToken<Brush>(token);

    /// <summary>The <c>radius-*</c> half of the same dictionary.</summary>
    private static CornerRadius ResolveTokenRadius(string token) => ResolveToken<CornerRadius>(token);

    private static T ResolveToken<T>(string token)
    {
        var resources = Application.Current?.Resources;
        if (resources is null || !resources.TryGetValue(token, out var value) || value is not T resolved)
        {
            throw new KeyNotFoundException(
                $"Theme/Tokens.xaml has no {typeof(T).Name} resource `{token}`");
        }

        return resolved;
    }
}
