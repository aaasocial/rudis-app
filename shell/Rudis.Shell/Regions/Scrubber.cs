using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation.Peers;
using Microsoft.UI.Xaml.Automation.Provider;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using Windows.System;

namespace Rudis.Shell.Regions;

/// <summary>
/// <c>Transport.Scrubber</c> — a REAL range control, which is UI-SPEC §6.5's
/// <b>deliberate divergence #1</b> and an intentional improvement over the Tauri
/// shell's pointer-driven <c>&lt;div&gt;</c>: it exposes
/// <see cref="PatternInterface.RangeValue"/> (read <i>and</i> set) through
/// <see cref="ScrubberAutomationPeer"/>, which makes SEEKING UIA-drivable — exactly
/// what SHELL-07's harness (plan 50-08) needs and what a div could never give it.
///
/// <para><b>Why a hand-written <see cref="Control"/> rather than a
/// <c>Slider</c>:</b> UI-SPEC §4 rule 3 requires the playhead to move by
/// TRANSFORM arithmetic and never by re-created geometry, because this element is
/// driven from the composition tick where SC-3's zero-allocation gate applies. A
/// <c>Slider</c> positions its own thumb through undocumented template parts and its
/// internal layout pass; owning the two numbers (played width + dot translate)
/// outright is what makes the hot path auditable. The cost — one automation peer
/// written by hand — is paid once, here.</para>
///
/// <para><b>Hot-path discipline.</b> <see cref="SetPositionFromEngine"/> is reachable
/// from <c>CompositionTarget.Rendering</c>. It early-returns unless the dot's WHOLE
/// PIXEL offset changes, so a sub-pixel playhead move touches nothing at all. The
/// visual-tree writes it does perform when the pixel does change are ordinary
/// dependency-property sets (WinUI has no allocation-free double DP set — the honest
/// limit is documented in <c>HotPathAllocationTests</c>).</para>
///
/// <para>Values are microseconds, carried as <see cref="double"/> because that is what
/// UIA's <c>RangeValuePattern</c> speaks. Plain CLR properties, deliberately NOT
/// dependency properties: nothing binds to them, and a DP set on the tick path would
/// box a double for no benefit.</para>
/// </summary>
public sealed class Scrubber : Control
{
    private const double DotSize = 10;
    private const double DotSizeScrubbing = 12;

    private Border? _track;
    private Border? _played;
    private Ellipse? _dot;
    private TranslateTransform? _dotTransform;

    private double _minimum;
    private double _maximum;
    private double _value;

    /// <summary>The last rendered whole-pixel dot offset; <c>-1</c> forces the next
    /// render. This is the hot path's change detector (UI-SPEC §4 rule 1 applied to
    /// geometry rather than to text).</summary>
    private double _renderedPixel = -1;

    private bool _dragging;

    public Scrubber()
    {
        // No DefaultStyleKey / Themes/Generic.xaml: the template is an EXPLICIT
        // token-only Style in Transport.xaml (50-05's established region pattern —
        // explicit templates so nothing can inherit a WinUI system brush, which would
        // be raw hex by proxy and would make the rule-7 gate meaningless).
        IsTabStop = true;
        UseSystemFocusVisuals = true;
        SizeChanged += (_, _) => Render(force: true);
        IsEnabledChanged += (_, _) => ApplyEnabledVisual();
    }

    /// <summary>The user dragged or clicked the track: a live <c>seek</c> in µs.</summary>
    public event Action<long>? UserSeekRequested;

    /// <summary>Left/Right on a focused scrubber = ±1 frame (a <c>step</c> command,
    /// UI-SPEC §5) — never a raw value nudge, because frame accuracy is the backend's
    /// to compute from the loaded media's fps.</summary>
    public event Action<int>? StepRequested;

    /// <summary>Home/End = project start / end.</summary>
    public event Action<bool>? JumpRequested;

    /// <summary>Drag started / ended, so the ticker can stop fighting the drag
    /// (frontend parity: the <c>scrubbing</c> guard, main.ts:670).</summary>
    public event Action<bool>? ScrubbingChanged;

    public double Minimum
    {
        get => _minimum;
        set
        {
            _minimum = value;
            Render(force: true);
        }
    }

    /// <summary>Duration in µs, from the MIRROR (cold path) — never invented here.</summary>
    public double Maximum
    {
        get => _maximum;
        set
        {
            _maximum = value;
            Render(force: true);
        }
    }

    public double Value
    {
        get => _value;
        set
        {
            _value = Clamp(value);
            Render(force: false);
        }
    }

    /// <summary>One frame at the loaded media's fps, in µs — used only for
    /// <see cref="IRangeValueProvider.SmallChange"/> so a UIA client sees a sane
    /// granularity. Zero when nothing is loaded.</summary>
    public double FrameStepUs { get; set; }

    public bool IsScrubbing => _dragging;

    /// <summary>
    /// HOT PATH (called from the composition tick through
    /// <c>PlayheadTicker</c>): move the playhead. Zero work unless the WHOLE PIXEL
    /// offset changes.
    /// </summary>
    public void SetPositionFromEngine(long positionUs)
    {
        if (_dragging)
        {
            // The user owns the playhead while dragging (main.ts:670 parity).
            return;
        }
        _value = Clamp(positionUs);
        Render(force: false);
    }

    /// <summary>Adopt a position without any seek side effect (cold path: the mirror
    /// said so).</summary>
    public void SetPositionFromMirror(long positionUs)
    {
        _value = Clamp(positionUs);
        Render(force: true);
    }

    protected override void OnApplyTemplate()
    {
        base.OnApplyTemplate();
        _track = GetTemplateChild("TrackRect") as Border;
        _played = GetTemplateChild("PlayedRect") as Border;
        _dot = GetTemplateChild("Dot") as Ellipse;
        if (_dot is not null)
        {
            _dotTransform = new TranslateTransform();
            _dot.RenderTransform = _dotTransform;
        }
        ApplyEnabledVisual();
        Render(force: true);
    }

    protected override AutomationPeer OnCreateAutomationPeer() => new ScrubberAutomationPeer(this);

    protected override void OnPointerPressed(PointerRoutedEventArgs e)
    {
        base.OnPointerPressed(e);
        if (!IsEnabled || _track is null)
        {
            return;
        }
        Focus(FocusState.Pointer);
        if (CapturePointer(e.Pointer))
        {
            SetDragging(true);
        }
        SeekToPointer(e);
        e.Handled = true;
    }

    protected override void OnPointerMoved(PointerRoutedEventArgs e)
    {
        base.OnPointerMoved(e);
        if (_dragging)
        {
            SeekToPointer(e);
            e.Handled = true;
        }
    }

    protected override void OnPointerReleased(PointerRoutedEventArgs e)
    {
        base.OnPointerReleased(e);
        if (_dragging)
        {
            ReleasePointerCapture(e.Pointer);
            SetDragging(false);
            e.Handled = true;
        }
    }

    protected override void OnPointerCaptureLost(PointerRoutedEventArgs e)
    {
        base.OnPointerCaptureLost(e);
        SetDragging(false);
    }

    protected override void OnKeyDown(KeyRoutedEventArgs e)
    {
        base.OnKeyDown(e);
        if (!IsEnabled)
        {
            return;
        }
        switch (e.Key)
        {
            case VirtualKey.Left:
                StepRequested?.Invoke(-1);
                e.Handled = true;
                break;
            case VirtualKey.Right:
                StepRequested?.Invoke(1);
                e.Handled = true;
                break;
            case VirtualKey.Home:
                JumpRequested?.Invoke(false);
                e.Handled = true;
                break;
            case VirtualKey.End:
                JumpRequested?.Invoke(true);
                e.Handled = true;
                break;
            default:
                break;
        }
    }

    private double Clamp(double value)
    {
        if (_maximum <= _minimum)
        {
            return _minimum;
        }
        return Math.Clamp(value, _minimum, _maximum);
    }

    private void SetDragging(bool dragging)
    {
        if (_dragging == dragging)
        {
            return;
        }
        _dragging = dragging;
        // §5 "scrubbing: dot enlarges, readout follows".
        if (_dot is not null)
        {
            _dot.Width = dragging ? DotSizeScrubbing : DotSize;
            _dot.Height = _dot.Width;
        }
        Render(force: true);
        ScrubbingChanged?.Invoke(dragging);
    }

    private void SeekToPointer(PointerRoutedEventArgs e)
    {
        if (_track is null || _track.ActualWidth <= 0 || _maximum <= _minimum)
        {
            return;
        }
        var x = e.GetCurrentPoint(_track).Position.X;
        var fraction = Math.Clamp(x / _track.ActualWidth, 0, 1);
        var target = _minimum + (fraction * (_maximum - _minimum));
        _value = target;
        Render(force: false);
        UserSeekRequested?.Invoke((long)Math.Round(target));
    }

    /// <summary>UIA <c>RangeValuePattern.SetValue</c> — a real seek, so a UIA client
    /// can drive the playhead exactly as a pointer drag does (§6.5).</summary>
    internal void SetValueFromAutomation(double value)
    {
        if (!IsEnabled)
        {
            return;
        }
        _value = Clamp(value);
        Render(force: false);
        UserSeekRequested?.Invoke((long)Math.Round(_value));
    }

    private void ApplyEnabledVisual()
    {
        if (_played is null || _dot is null)
        {
            return;
        }
        // §5 disabled row: everything drops to text-tertiary. Tokens only (rule 7).
        var resources = Application.Current.Resources;
        _played.Background = (Brush)resources[IsEnabled ? "on-accent" : "text-tertiary"];
        _dot.Fill = (Brush)resources[IsEnabled ? "scrub-dot" : "text-tertiary"];
        _dot.Stroke = (Brush)resources[IsEnabled ? "on-accent" : "border-strong"];
    }

    /// <summary>
    /// The ONE place geometry moves. UI-SPEC §4 rule 3: a translate on the dot plus a
    /// width on the played portion — never a re-created shape, path or brush.
    /// </summary>
    private void Render(bool force)
    {
        if (_track is null || _played is null || _dotTransform is null)
        {
            return;
        }
        var width = _track.ActualWidth;
        if (width <= 0)
        {
            return;
        }
        var span = _maximum - _minimum;
        var fraction = span > 0 ? Math.Clamp((_value - _minimum) / span, 0, 1) : 0;
        var pixel = Math.Round(fraction * width);
        if (!force && pixel == _renderedPixel)
        {
            // The hot path's early return: a sub-pixel playhead move touches NOTHING.
            return;
        }
        _renderedPixel = pixel;
        _played.Width = pixel;
        _dotTransform.X = pixel - ((_dot?.Width ?? DotSize) / 2);
    }
}

/// <summary>
/// The automation peer that makes <see cref="Scrubber"/> a first-class UIA range
/// (UI-SPEC §6: "expose <c>RangeValue</c> so UIA can read/set position", §8's
/// "<c>Transport.Scrubber</c> exposes <c>RangeValuePattern</c> (read + set)").
///
/// <para>Reported as <see cref="AutomationControlType.Slider"/> deliberately: a UIA
/// client looking for a seek control looks for a Slider, and SHELL-07's harness should
/// not need to know that this one is hand-written.</para>
/// </summary>
internal sealed class ScrubberAutomationPeer(Scrubber owner)
    : FrameworkElementAutomationPeer(owner), IRangeValueProvider
{
    private Scrubber Scrubber => (Scrubber)Owner;

    public bool IsReadOnly => !Scrubber.IsEnabled;

    public double LargeChange => Math.Max(Scrubber.FrameStepUs * 10, 1_000_000);

    public double Maximum => Scrubber.Maximum;

    public double Minimum => Scrubber.Minimum;

    public double SmallChange => Scrubber.FrameStepUs > 0 ? Scrubber.FrameStepUs : 1_000;

    public double Value => Scrubber.Value;

    public void SetValue(double value) => Scrubber.SetValueFromAutomation(value);

    protected override object GetPatternCore(PatternInterface patternInterface)
        => patternInterface == PatternInterface.RangeValue ? this : base.GetPatternCore(patternInterface);

    protected override AutomationControlType GetAutomationControlTypeCore()
        => AutomationControlType.Slider;

    protected override string GetClassNameCore() => nameof(Scrubber);
}
