using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Windows.Graphics;

namespace Rudis.Shell.Regions;

/// <summary>
/// The <c>TitleBar</c> region (design_handoff_rudis_editor/README.md:81 — the name
/// is the handoff's, verbatim, per CLAUDE.md rule 7).
///
/// <para>Owns three things and no state of its own: the window's custom chrome
/// mechanics, the three window controls, and a READ-ONLY rendering of the project
/// the backend actually holds (rule 4 — <see cref="SetProject"/> is a sink, never a
/// source).</para>
/// </summary>
public sealed partial class TitleBar : UserControl
{
    /// <summary><c>ChromeMaximize</c> (U+E922) - the handoff's maximize square.
    /// Written as an escape so this file stays pure ASCII.</summary>
    private const string MaximizeGlyph = "\uE922";

    /// <summary><c>ChromeRestore</c> (U+E923) — shown while maximized so
    /// the glyph tells the truth about what the button will do.</summary>
    private const string RestoreGlyph = "\uE923";

    private Window? _window;
    private AppWindow? _appWindow;
    private OverlappedPresenter? _presenter;
    private bool _dragRectanglesLive;

    public TitleBar()
    {
        InitializeComponent();
    }

    /// <summary>
    /// What the custom-chrome configuration ACTUALLY achieved on this machine, as a
    /// one-line human string. Surfaced in the window's status readout on purpose:
    /// UI-SPEC §9 item 9 marks the WinUI 3 title-bar specifics UNVERIFIED, so the
    /// launch screenshot has to be able to show which mechanism took effect rather
    /// than the plan asserting one.
    /// </summary>
    public string MechanismNote { get; private set; } = "title bar not configured";

    /// <summary>
    /// What this region does when its close button is clicked. Installed ONCE by
    /// <c>MainWindow</c> (plan 60.1-06).
    ///
    /// <para>A settable delegate rather than an event, and the difference is the
    /// whole point: an event can be subscribed twice with <c>+=</c>, and a second
    /// subscriber IS a second close path — the precise defect this plumbing exists to
    /// make impossible. One slot, one owner. The file's existing shape for a
    /// window-installed callback is <c>Chat.SelectionProvider</c>, wired the same way
    /// from the same constructor.</para>
    ///
    /// <para>Null until <c>MainWindow</c> installs it, and the button then does
    /// nothing — deliberately. A close that skipped the funnel would be a close that
    /// skipped the save, and this region no longer holds the means to perform one
    /// either way: it has no <c>AppWindow</c> destroy call of its own any more.</para>
    /// </summary>
    public Action? RequestClose { get; set; }

    /// <summary>
    /// Asks <c>MainWindow</c> to open Settings (Phase 69, D-69-10) — the app menu's one
    /// item. Installed by <c>MainWindow</c> alongside <see cref="RequestClose"/>; it routes
    /// to <c>MainWindow.ShowSettingsAsync</c>, the same guarded entry <c>Ctrl+,</c> and
    /// <c>Chat.KeyButton</c> use. Null until installed, and the menu item then does nothing.
    /// </summary>
    public Action? RequestSettings { get; set; }

    private void OnSettingsClick(object sender, RoutedEventArgs e) => RequestSettings?.Invoke();

    /// <summary>
    /// Bind this region to its window. Called once by <c>MainWindow</c>.
    ///
    /// <para><b>UI-SPEC §9 item 9 marked the WinUI 3 custom-title-bar mechanism
    /// UNVERIFIED. What follows is what was MEASURED on 2026-07-30 — WinAppSDK
    /// 1.8.260710003, .NET 9.0.316, Windows 10 19045, unpackaged — with the full
    /// transcript in artifacts/50-05-regions.md. It is not transcribed from a doc.</b></para>
    /// <list type="number">
    /// <item><c>AppWindow.TitleBar.ExtendsContentIntoTitleBar = true</c> is the
    ///   precondition for <c>SetDragRectangles</c>; on its own it also leaves the
    ///   SYSTEM caption buttons drawn.</item>
    /// <item><c>OverlappedPresenter.SetBorderAndTitleBar(hasBorder: true,
    ///   hasTitleBar: false)</c>, applied AFTER (1), removes the system title bar and
    ///   its system caption buttons while KEEPING the resize border — <b>and the drag
    ///   rectangles survive it.</b> That composition is the entire trick, and it is why
    ///   the three buttons in this region's XAML are the window's only caption
    ///   buttons: SHELL-07's UIA harness can only drive controls that carry an
    ///   AutomationId this codebase owns, and the system buttons do not.</item>
    /// <item>The presenter still reports <c>IsMinimizable</c> and
    ///   <c>IsMaximizable</c> true afterwards, and both operations work through this
    ///   region's buttons — measured by synthetic click, so removing the system
    ///   title bar costs no window capability.</item>
    /// <item><c>SetDragRectangles</c> takes PHYSICAL pixels, so each rectangle is
    ///   scaled by <c>XamlRoot.RasterizationScale</c> and recomputed on
    ///   <c>SizeChanged</c> (either panel) and on <c>XamlRoot.Changed</c>, which is
    ///   the scale/DPI signal. Without the recompute the drag area desyncs from the
    ///   visuals exactly as UI-SPEC §2 warns; the launch check drags AGAIN after a
    ///   maximize/restore round trip specifically to catch that.</item>
    /// <item>The buttons sit OUTSIDE every drag rectangle (the rectangle ends
    ///   <c>WindowControls.ActualWidth</c> short of the right edge). This is the
    ///   classic failure UI-SPEC §2 names — an interactive child inside a drag
    ///   rectangle stops receiving clicks — and it is verified BY INPUT: the launch
    ///   check synthesises a click on Minimize and asserts the window becomes iconic,
    ///   then on Maximize and asserts it becomes zoomed.</item>
    /// </list>
    ///
    /// <para>Fallback, kept deliberately: if <c>SetDragRectangles</c> ever throws (a
    /// different Windows build, a future SDK), <c>Window.SetTitleBar</c> is used on
    /// the empty filler element instead and <see cref="MechanismNote"/> says so. A
    /// window that cannot be moved is a worse outcome than a second code path.</para>
    /// </summary>
    public void AttachToWindow(Window window)
    {
        _window = window;
        _appWindow = window.AppWindow;
        _presenter = _appWindow.Presenter as OverlappedPresenter;

        var notes = new List<string>();

        // (2) before (1): ExtendsContentIntoTitleBar is the precondition for drag
        // rectangles; removing the title bar afterwards keeps them without drawing
        // the system caption buttons.
        try
        {
            _appWindow.TitleBar.ExtendsContentIntoTitleBar = true;
            notes.Add("ExtendsContentIntoTitleBar=true");
        }
        catch (Exception e)
        {
            notes.Add($"ExtendsContentIntoTitleBar FAILED ({e.GetType().Name})");
        }

        if (_presenter is not null)
        {
            try
            {
                _presenter.SetBorderAndTitleBar(true, false);
                // MEASURED, not assumed: after this call IsMinimizable and
                // IsMaximizable are BOTH still true (the launch note renders them,
                // and the synthetic-click checks minimise and maximise the window
                // through this region's own buttons). So removing the system title
                // bar costs no window capability, and no flag needs re-asserting.
                notes.Add($"SetBorderAndTitleBar(border,noTitleBar) min={_presenter.IsMinimizable} max={_presenter.IsMaximizable}");
            }
            catch (Exception e)
            {
                notes.Add($"SetBorderAndTitleBar FAILED ({e.GetType().Name})");
            }
        }
        else
        {
            notes.Add("presenter is not OverlappedPresenter");
        }

        TitleBarRoot.SizeChanged += (_, _) => UpdateDragRegion();
        WindowControls.SizeChanged += (_, _) => UpdateDragRegion();
        Loaded += OnLoadedFirstTime;

        MechanismNote = string.Join(" · ", notes);
        UpdateMaximizeGlyph();
    }

    /// <summary>
    /// Render the project the mirror holds. Empty name → the handoff's no-project
    /// state; unsaved → a trailing <c>•</c> (handoff README:84, UI-SPEC §5 row 3).
    /// </summary>
    /// <param name="name">The backend's <c>project.name</c>; empty when never named.</param>
    /// <param name="unsaved">
    /// Whether the backend holds mutations no persistence hook has written. Derived
    /// by the caller from mirrored state only — see <c>MainWindow.HasUnpersistedEdits</c>.
    /// </param>
    public void SetProject(string? name, bool unsaved)
    {
        var label = string.IsNullOrWhiteSpace(name) ? "— No project" : $"— {name}";
        if (unsaved)
        {
            label += " •";
        }
        ProjectNameText.Text = label;
        // UIA reads Name, not Text, for a live-region announcement (UI-SPEC §6:
        // "Name = filename; live-region on change").
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(
            ProjectNameText, string.IsNullOrWhiteSpace(name) ? "No project" : name);
    }

    /// <summary>
    /// Inactive-window dimming (UI-SPEC §5 row 2): text and control glyphs drop to
    /// <c>text-tertiary</c> when the window is deactivated. Tokens only — the brushes
    /// are looked up by the handoff's token names, so no colour literal exists here.
    /// </summary>
    public void SetWindowActive(bool active)
    {
        var primary = Token(active ? "text-primary" : "text-tertiary");
        var glyph = Token(active ? "text-secondary" : "text-tertiary");
        var brand = Token(active ? "accent" : "text-tertiary");

        AppMenuButton.Foreground = brand;
        AppNameText.Foreground = primary;
        ProjectNameText.Foreground = primary;
        MinimizeButton.Foreground = glyph;
        MaximizeButton.Foreground = glyph;
        CloseButton.Foreground = glyph;
    }

    private static Brush Token(string key) => (Brush)Application.Current.Resources[key];

    private void OnLoadedFirstTime(object sender, RoutedEventArgs e)
    {
        Loaded -= OnLoadedFirstTime;

        // XamlRoot exists only once loaded. Its Changed event is the scale/DPI
        // signal — a monitor change or a scale change must recompute the physical-
        // pixel drag rectangles or the drag area drifts off the visuals.
        if (XamlRoot is not null)
        {
            XamlRoot.Changed += (_, _) => UpdateDragRegion();
        }

        UpdateDragRegion();
        SetWindowActive(true);
    }

    /// <summary>
    /// Recompute the drag region from the LAID-OUT geometry, in physical pixels,
    /// excluding the window controls. Called on first load, on either panel's
    /// SizeChanged, and on XamlRoot scale changes.
    ///
    /// <para>Phase 69: the app-menu button (<c>TitleBar.AppMenu</c>) is the one
    /// interactive element inside <c>BrandBlock</c>, so the rectangle now STARTS to the
    /// right of it (plus 4 logical px of slack) — otherwise the caption drag would swallow
    /// its click (T-69-27). The fallback path (<c>SetTitleBar(DragFiller)</c>) already
    /// leaves interactive siblings clickable.</para>
    /// </summary>
    private void UpdateDragRegion()
    {
        if (_appWindow is null)
        {
            return;
        }

        var scale = XamlRoot?.RasterizationScale ?? 1.0;
        var barWidth = TitleBarRoot.ActualWidth;
        var barHeight = TitleBarRoot.ActualHeight;
        var controlsWidth = WindowControls.ActualWidth;
        if (barWidth <= 0 || barHeight <= 0)
        {
            return;
        }

        var dragWidth = Math.Max(0.0, barWidth - controlsWidth);

        var menuRight = 0.0;
        if (AppMenuButton.ActualWidth > 0)
        {
            var origin = AppMenuButton.TransformToVisual(TitleBarRoot)
                .TransformPoint(new Windows.Foundation.Point(0, 0));
            menuRight = origin.X + AppMenuButton.ActualWidth + 4; // 4 logical px of slack so the edge is not a drag
        }
        var dragLeft = Math.Min(menuRight, dragWidth);

        var rect = new RectInt32(
            (int)Math.Round(dragLeft * scale),
            0,
            (int)Math.Round((dragWidth - dragLeft) * scale),
            (int)Math.Round(barHeight * scale));

        try
        {
            _appWindow.TitleBar.SetDragRectangles([rect]);
            if (!_dragRectanglesLive)
            {
                _dragRectanglesLive = true;
                MechanismNote += " · SetDragRectangles LIVE · app-menu excluded";
            }
        }
        catch (Exception e)
        {
            // Documented fallback (see AttachToWindow's remarks): designate the empty
            // filler as the drag element instead. Interactive siblings keep their
            // clicks because they are not inside the designated element.
            if (!_dragRectanglesLive && _window is not null)
            {
                try
                {
                    _window.SetTitleBar(DragFiller);
                    _dragRectanglesLive = true;
                    MechanismNote += $" · SetDragRectangles threw ({e.GetType().Name}) → Window.SetTitleBar(DragFiller)";
                }
                catch (Exception inner)
                {
                    MechanismNote += $" · NO drag mechanism ({inner.GetType().Name})";
                }
            }
        }
    }

    private void UpdateMaximizeGlyph()
    {
        var maximized = _presenter?.State == OverlappedPresenterState.Maximized;
        MaximizeButton.Content = maximized ? RestoreGlyph : MaximizeGlyph;
    }

    private void OnMinimizeClick(object sender, RoutedEventArgs e) => _presenter?.Minimize();

    private void OnMaximizeClick(object sender, RoutedEventArgs e) => ToggleMaximize();

    /// <summary>
    /// CLOSE CONTRACT, rewritten by plan 60.1-06: <b>this button no longer closes the
    /// window.</b> It ASKS, through <see cref="RequestClose"/>, and
    /// <c>MainWindow.RequestCloseAsync</c> is the one code path that saves the
    /// project, releases the GPU surface and only then destroys the window.
    ///
    /// <para><b>Why this region cannot own the close any more.</b> The old body was
    /// <c>_appWindow?.Destroy()</c>, and Microsoft Learn is explicit that <i>"the
    /// Closing event does not occur when the AppWindow.Destroy method is called"</i>.
    /// <c>AppWindow.Closing</c> is the only seam early enough to save at all — so a
    /// save installed there would have run for Alt+F4 and the system menu and NEVER
    /// for the button the user actually clicks. Two close paths, one of them silent,
    /// and the silent one is the common one. That is the failure PROJ-01 exists to
    /// end, so it may not be reintroduced one layer up.</para>
    ///
    /// <para><b>What the retired contract said, and why it WAS right.</b> It read:
    /// <i>"parity — no extra call (artifacts/50-02-unknowns.md §2.3) … there is NO
    /// explicit-save export among the 23, and no explicit-save command on the Tauri
    /// side either. Adding one would be a Rust-side widening and must be its own
    /// planned change (D-15)."</i> That was exact when it was written: with nothing to
    /// call, asking the window to close was the honest whole of the contract, and the
    /// unsaved dot was the honest surface for what could not be saved. <b>Phase 60.1
    /// IS that planned change</b> — <c>rudis_save_project</c> now exists — so the
    /// condition the old note itself named has been met and the contract changed with
    /// it. The sentence is kept rather than deleted because a reader who finds only
    /// the new rule cannot tell whether the old one was wrong or merely superseded.</para>
    /// </summary>
    private void OnCloseClick(object sender, RoutedEventArgs e) => RequestClose?.Invoke();

    /// <summary>
    /// Double-click on the EMPTY drag area maximizes/restores (OS convention,
    /// UI-SPEC §3). Measured on 2026-07-30: with drag rectangles live the OS already
    /// provides this inside the rectangle, so this handler is the belt-and-braces
    /// path for the <c>SetTitleBar</c> fallback and for the brand/project text, which
    /// is display-only and sits outside the filler.
    /// </summary>
    private void OnDragAreaDoubleTapped(object sender, DoubleTappedRoutedEventArgs e) => ToggleMaximize();

    private void ToggleMaximize()
    {
        if (_presenter is null)
        {
            return;
        }
        if (_presenter.State == OverlappedPresenterState.Maximized)
        {
            _presenter.Restore();
        }
        else
        {
            _presenter.Maximize();
        }
        UpdateMaximizeGlyph();
        UpdateDragRegion();
    }
}
