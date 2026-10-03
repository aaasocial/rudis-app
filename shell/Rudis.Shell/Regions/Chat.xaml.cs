using System.Text.Json;
using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Rudis.Shell.Interop;
using Windows.System;
using Windows.UI.Core;

namespace Rudis.Shell.Regions;

/// <summary>
/// The <c>Chat</c> region (design_handoff_rudis_editor/README.md:142 — the name is the
/// handoff's, verbatim, per CLAUDE.md rule 7).
///
/// <para><b>ZERO NEW INTEROP.</b> All five exports this region drives were already
/// wrapped before the phase started (<c>Interop/RudisNative.cs:203-230</c>):
/// <c>rudis_agent_send_message</c>, <c>rudis_agent_status</c>,
/// <c>rudis_apply_option_card</c>, <c>rudis_set_api_key</c>,
/// <c>rudis_clear_api_key</c>. A step here that seems to need new P/Invoke has misread
/// the seam (Phase 53 D-11). <c>ApplyOptionCardAsync</c> gets its FIRST caller in this
/// file.</para>
///
/// <para><b>THIS REGION HOLDS NO AUTHORITATIVE STATE</b> (CLAUDE.md rule 4). The
/// transcript is view state. Everything an agent turn CHANGES about the project reaches
/// the Timeline and MediaBin through the mirror, and this file has no mirror
/// subscription at all — see the comment on <see cref="ApplyCardAsync"/>, which is the
/// place the temptation actually arises.</para>
///
/// <para><b>Threading.</b> Every handler starts on the UI thread; every awaited call is
/// posted to the ONE interop worker by <see cref="RudisNative"/> and its continuation is
/// restored to the UI thread by WinUI's SynchronizationContext — the pattern all five
/// prior regions use. No <c>async void</c>, and no blocking wait anywhere (both are
/// mechanical gates, not conventions).</para>
///
/// <para>Every DECISION this file renders was made in the WinUI-free half beside it
/// (<c>Regions/Chat/ChatTurnPresenter.cs</c>, plan 54-01), where it is pinned by 38 unit
/// cases with no window. This file only draws.</para>
/// </summary>
public sealed partial class Chat : UserControl
{
    /// <summary>v6.0's in-flight copy (<c>main.ts:2199</c>), verbatim.
    ///
    /// <para>The animated ellipsis v6.0 paired with it was CSS sugar (an
    /// <c>@keyframes</c> rule on <c>.chat-running::after</c>); the COPY is the contract
    /// and the animation is not ported. If one is ever wanted it must be storyboard
    /// driven — never a <c>CompositionTarget.Rendering</c> subscription. The shell has
    /// exactly one per-tick call site and a chat spinner is emphatically not it.</para>
    /// </summary>
    private const string ThinkingCopy = "Thinking…";

    /// <summary>The ~15s escalation copy (<c>main.ts:2201-2202</c>), verbatim, so a slow
    /// vision turn never reads as frozen.</summary>
    private const string StillThinkingCopy =
        "Still thinking… turns with a sketch attached can take a couple of minutes";

    /// <summary>How long before the in-flight copy escalates. v6.0's 15000ms.</summary>
    private static readonly TimeSpan EscalateAfter = TimeSpan.FromSeconds(15);

    /// <summary>v6.0's <c>chatSending</c> re-entrancy guard (<c>main.ts:2183</c>).</summary>
    private bool _sending;

    /// <summary>Mirrors the last <c>agent_status</c>'s <c>key_configured</c>. Drives the
    /// pill and the key button's label.</summary>
    private bool _connected;

    /// <summary>The card elements currently in the transcript, so
    /// <see cref="ClearCards"/> can remove exactly those and nothing else — the
    /// transcript is shared with the bubbles, unlike v6.0's dedicated cards mount whose
    /// innerHTML could simply be emptied.</summary>
    private readonly List<FrameworkElement> _cardElements = [];

    /// <summary>Monotonic index behind every bubble's AutomationId.</summary>
    private int _bubbleIndex;

    public Chat() => InitializeComponent();

    /// <summary>
    /// Supplies the turn's <c>selection</c> argument — v6.0 parity
    /// (<c>main.ts:2208</c>): the selected clip id, or <c>null</c> for none, which
    /// <see cref="ChatCommandPayloads.SendMessage"/> turns into a one-element or empty
    /// array. Plan 54-05 wires this to the Timeline's selection when it mounts the
    /// region; unset, every turn simply carries an empty selection.
    /// </summary>
    public Func<string?>? SelectionProvider { get; set; }

    /// <summary>
    /// Opens Settings — the one key surface since Phase 69 (D-69-10). Installed by
    /// <c>MainWindow</c> (it routes to <c>MainWindow.ShowSettingsAsync</c>, the same entry
    /// the TitleBar app menu and <c>Ctrl+,</c> use). Unset, <c>Chat.KeyButton</c> does
    /// nothing — deliberately: the region has no dialog root of its own.
    /// </summary>
    public Action? RequestSettings { get; set; }

    // ════════════════════════════════════════════════════════════════════════
    // The transcript — ONE writer
    // ════════════════════════════════════════════════════════════════════════

    /// <summary>
    /// The ONLY thing that writes a bubble into the transcript (T-54-04). Model- and
    /// backend-authored text reaches the screen exclusively through
    /// <c>TextBlock.Text</c> — never a markup path, never a dynamic XAML parse. That is
    /// the WinUI half of v6.0's <c>textContent</c>-never-<c>innerHTML</c> rule
    /// (<c>main.ts:2167</c>); the difference is that WinUI does not interpret markup in
    /// <c>Text</c> at all, so the property choice is the whole mitigation.
    ///
    /// <para>The bubble's KIND is carried losslessly to UIA by its AutomationId prefix,
    /// which matters because the visual difference between an agent bubble and an error
    /// bubble is only an outline (the handoff defines no error token — see Chat.xaml).
    /// Errors are findable as <c>Chat.ErrorBubble.*</c> and generation disclosures as
    /// <c>Chat.DisclosureBubble.*</c>; the latter is D-18's REQUIRED axis, so making it
    /// mechanically findable is what lets a later proof assert the billing disclosures
    /// are present rather than eyeball them.</para>
    /// </summary>
    private Border AppendBubble(ChatBubbleKind kind, string text, string? automationId = null)
    {
        var (borderStyle, textStyle, idPrefix) = kind switch
        {
            ChatBubbleKind.User => ("ChatBubbleUser", "ChatBubbleTextOnAccent", "Chat.Bubble"),
            ChatBubbleKind.Disclosure => ("ChatBubbleDisclosure", "ChatBubbleTextDisclosure", "Chat.DisclosureBubble"),
            ChatBubbleKind.Error => ("ChatBubbleError", "ChatBubbleText", "Chat.ErrorBubble"),
            _ => ("ChatBubbleAgent", "ChatBubbleText", "Chat.Bubble"),
        };

        var label = new TextBlock
        {
            Style = (Style)Resources[textStyle],
            Text = text,
        };

        var bubble = new Border
        {
            Style = (Style)Resources[borderStyle],
            Child = label,
        };

        AutomationProperties.SetAutomationId(bubble, automationId ?? $"{idPrefix}.{_bubbleIndex++}");
        AutomationProperties.SetName(bubble, text);

        EmptyState.Visibility = Visibility.Collapsed;
        TranscriptPanel.Children.Add(bubble);
        ScrollToEnd();
        return bubble;
    }

    /// <summary>Pin the transcript to its newest line. The layout pass is forced first
    /// because <c>ScrollableHeight</c> is stale until the just-added child has been
    /// measured.</summary>
    private void ScrollToEnd()
    {
        TranscriptPanel.UpdateLayout();
        TranscriptScroll.ChangeView(null, TranscriptScroll.ScrollableHeight, null);
    }

    // ════════════════════════════════════════════════════════════════════════
    // The turn — a port of v6.0's sendChatMessage (main.ts:2184-2315)
    // ════════════════════════════════════════════════════════════════════════

    // Sync handler starting an async Task that carries its own total try/catch — the
    // async-void-free way to run work from an event (the shape every region since 50-04
    // uses; a grep for `async void` over shell/ must stay at zero).
    private void OnSendClick(object sender, RoutedEventArgs e) => _ = SendAsync();

    /// <summary>
    /// One agent turn, end to end: echo the user, show an in-flight affordance, call
    /// <c>rudis_agent_send_message</c>, then render whatever
    /// <see cref="ChatTurnPresenter"/> decides the answer is.
    ///
    /// <para><b>Errors are surfaced VERBATIM</b> and never swallowed (D-17). A missing
    /// key, a provider refusal and a transport fault all reach the transcript as the
    /// backend's own words, because the twenty-odd real causes are distinguishable only
    /// by their message.</para>
    /// </summary>
    internal async Task SendAsync()
    {
        if (_sending)
        {
            return;
        }

        var message = ComposerInput.Text?.Trim();
        if (string.IsNullOrEmpty(message))
        {
            return;
        }

        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        _sending = true;
        AppendBubble(ChatBubbleKind.User, message);
        ComposerInput.Text = string.Empty;
        ComposerInput.IsEnabled = false;
        SendButton.IsEnabled = false;

        // The in-flight affordance. NOT a token-streaming caret: the turn is one-shot
        // (Assumption A4), so there is nothing mid-turn to stream.
        var running = AppendBubble(ChatBubbleKind.Agent, ThinkingCopy, "Chat.RunningBubble");
        var runningLabel = (TextBlock)running.Child;

        var escalate = DispatcherQueue.CreateTimer();
        escalate.Interval = EscalateAfter;
        escalate.IsRepeating = false;
        escalate.Tick += (_, _) =>
        {
            runningLabel.Text = StillThinkingCopy;
            AutomationProperties.SetName(running, StillThinkingCopy);
        };
        escalate.Start();

        try
        {
            ChatTurnView view;
            try
            {
                var turn = await engine.AgentSendMessageAsync(
                    ChatCommandPayloads.SendMessage(message, SelectionProvider?.Invoke()));

                view = turn.Kind == RudisResultKind.Ok
                    ? ChatTurnPresenter.Present(turn.Value)
                    : ChatTurnPresenter.PresentError(
                        turn.Error ?? $"agent_send_message failed ({turn.Kind}/{turn.Status})");
            }
            catch (Exception ex)
            {
                // v6.0's catch branch: the throw itself is the message.
                view = ChatTurnPresenter.PresentError($"{ex.GetType().Name}: {ex.Message}");
            }

            // Ordering matters: the in-flight bubble comes off BEFORE the reply goes on,
            // so the transcript never reads "Thinking…" above a finished answer.
            TranscriptPanel.Children.Remove(running);

            foreach (var bubble in view.Bubbles)
            {
                AppendBubble(bubble.Kind, bubble.Text);
            }

            // D-09: a non-card turn clears stale cards, because the backend has already
            // replaced `pending_option_choice` and their ids would now be refused with
            // "unknown option card id" — leaving them up offers a button that cannot
            // work. A FAILED turn does not clear them (PresentError sets ClearsCards
            // false): it never replaced the pending choice, so they are still live.
            if (view.ClearsCards)
            {
                ClearCards();
            }

            if (view.Cards.Count > 0)
            {
                RenderCards(view.Cards);
            }
        }
        finally
        {
            // T-54-05 / T-14.3-09: the timer is stopped on EVERY path — Ok, domain
            // error, transport fault, and a throw out of the render loop alike. A
            // one-shot DispatcherQueueTimer that already fired is harmless to stop;
            // one that has not is exactly the leak this finally exists for.
            escalate.Stop();

            // Idempotent by design (Remove answers false when it is already gone): the
            // inline removal above is the one that runs normally, and this is the net
            // for a throw between it and here, which would otherwise strand a
            // "Thinking…" bubble on screen forever.
            TranscriptPanel.Children.Remove(running);

            _sending = false;
            ComposerInput.IsEnabled = true;
            SendButton.IsEnabled = true;
            ComposerInput.Focus(FocusState.Programmatic);
        }
    }

    /// <summary>
    /// Enter sends; Shift+Enter inserts a newline (v6.0's <c>main.ts:2393-2399</c>).
    ///
    /// <para><b>⚠ MEASURED 2026-07-31 — and the assumed answer was WRONG, which is why
    /// the finding is written here rather than in a planning document.</b> 54-RESEARCH
    /// flagged this API as LOW confidence (Open Question 2 / Assumption A1) and the plan
    /// named <c>KeyDown</c> as the primary candidate with <c>PreviewKeyDown</c> as a
    /// fallback. A probe built against this exact WindowsAppSDK 1.8 / net9.0 target,
    /// with real keystrokes injected so the whole input stack ran, found:</para>
    ///
    /// <list type="bullet">
    /// <item><b><c>KeyDown</c> never fires for Enter at all</b> on a <c>TextBox</c> that
    ///   accepts returns, and the newline is inserted regardless. A
    ///   <c>KeyDown="…"</c> handler here would have been silently inert — a Send button
    ///   that works beside an Enter key that does nothing.</item>
    /// <item>The same handler re-registered with <c>handledEventsToo: true</c> DOES run,
    ///   and sees <c>e.Handled == true</c> already — so the TextBox marks Enter handled
    ///   before instance <c>KeyDown</c> handlers are invoked. That is the mechanism, and
    ///   even from there it is too late: the newline has landed.</item>
    /// <item><b><c>PreviewKeyDown</c> fires with <c>e.Handled == false</c>, and setting
    ///   it true SUPPRESSES the newline.</b> This is the correct interception point.</item>
    /// <item>With Shift held, the same handler declining leaves the newline inserted —
    ///   v6.0's behaviour exactly. <c>KeyRoutedEventArgs</c> carries no modifier state,
    ///   so the <c>InputKeyboardSource</c> query below is load-bearing, not a
    ///   convenience; it too was measured returning Down only for the Shift case.</item>
    /// </list>
    ///
    /// <para>Full transcript: <c>artifacts/54-03-winui-mechanics.md</c>. The next region
    /// with a multiline <c>TextBox</c> should copy the measured answer, not the
    /// plausible one.</para>
    ///
    /// <para>An Enter in an empty or whitespace-only composer is a NO-OP, because
    /// <see cref="SendAsync"/> trims and returns. Plan 54-05's UIA test depends on
    /// exactly that property to prove the interception without ever spending a paid
    /// turn.</para>
    /// </summary>
    private void OnComposerPreviewKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key != VirtualKey.Enter)
        {
            return;
        }

        if (InputKeyboardSource
            .GetKeyStateForCurrentThread(VirtualKey.Shift)
            .HasFlag(CoreVirtualKeyStates.Down))
        {
            // Shift+Enter: decline, and the TextBox inserts its newline as usual.
            return;
        }

        e.Handled = true;
        _ = SendAsync();
    }

    // ════════════════════════════════════════════════════════════════════════
    // Option cards — v6.0's renderOptionCards/clearOptionCards, re-homed by D-06
    // ════════════════════════════════════════════════════════════════════════

    /// <summary>Remove exactly this region's card elements from the transcript.</summary>
    private void ClearCards()
    {
        foreach (var element in _cardElements)
        {
            TranscriptPanel.Children.Remove(element);
        }

        _cardElements.Clear();
    }

    /// <summary>
    /// Draw one card per pending option, INSIDE the transcript directly below the reply
    /// that offered them (D-06 — v6.0 drew them in the Canvas region, and
    /// <c>index.html:154</c>'s "(not Chat)" comment is the OLD resolution, overturned).
    ///
    /// <para>Each card is ✦ + label + rationale + Apply and NOTHING else, which is also
    /// all v6.0 drew (<c>main.ts:2489-2541</c>). <see cref="ChatOptionCard"/> has no
    /// <c>tool</c>/<c>args</c> members to render even if someone wanted to: they are
    /// display-only on the wire and this port never deserializes them (D-07 / T-54-01,
    /// enforced at the type level in plan 54-01).</para>
    /// </summary>
    private void RenderCards(IReadOnlyList<ChatOptionCard> cards)
    {
        ClearCards();

        foreach (var card in cards)
        {
            var head = new Grid { ColumnSpacing = 6 };
            head.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            head.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });

            var mark = new TextBlock
            {
                Style = (Style)Resources["ChatOptionCardMark"],
                Text = "✦",
            };
            var label = new TextBlock
            {
                Style = (Style)Resources["ChatOptionCardLabel"],
                Text = card.Label,
            };
            Grid.SetColumn(label, 1);
            head.Children.Add(mark);
            head.Children.Add(label);

            var rationale = new TextBlock
            {
                Style = (Style)Resources["ChatOptionCardRationale"],
                Text = card.Rationale,
                Margin = new Thickness(0, 4, 0, 0),
            };

            var apply = new Button
            {
                Style = (Style)Resources["ChatAccentButtonStyle"],
                Content = "Apply",
                Height = 24,
                Margin = new Thickness(0, 7, 0, 0),
                HorizontalAlignment = HorizontalAlignment.Right,
            };
            AutomationProperties.SetAutomationId(apply, $"Chat.OptionCard.{card.Id}.Apply");
            AutomationProperties.SetName(apply, $"Apply {card.Label}");

            var cardId = card.Id;
            apply.Click += (_, _) => _ = ApplyCardAsync(apply, cardId);

            var body = new StackPanel();
            body.Children.Add(head);
            body.Children.Add(rationale);
            body.Children.Add(apply);

            var root = new Border
            {
                Style = (Style)Resources["ChatOptionCard"],
                Child = body,
            };
            AutomationProperties.SetAutomationId(root, $"Chat.OptionCard.{card.Id}");
            AutomationProperties.SetName(root, card.Label);

            EmptyState.Visibility = Visibility.Collapsed;
            TranscriptPanel.Children.Add(root);
            _cardElements.Add(root);
        }

        ScrollToEnd();
    }

#if DEBUG
    /// <summary>
    /// DEBUG-ONLY UIA scaffolding (<c>--synth-chat-cards</c>, plan 54-05): render two
    /// SYNTHETIC cards through the REAL card pipeline, so a real-mouse UIA click can
    /// drive the REAL <see cref="ApplyCardAsync"/> dispatch without a paid model turn.
    ///
    /// <para>The backend holds no <c>pending_option_choice</c> for these ids, so the
    /// click exercises the FULL pipe and ends in a refusal: OS input → button → payload
    /// builder → ABI → backend refusal → error bubble. That refusal IS the proof — it can
    /// only be produced by a request that really reached the backend, which is why this is
    /// worth more than a green assertion about a handler. 53.1-04's discipline: a
    /// handler-level substitute would have hidden exactly the class of failure
    /// MEDIABIN-53 found.</para>
    ///
    /// <para><b>It forks nothing.</b> <see cref="RenderCards"/> is the shipped path and
    /// this calls it; the cards differ from real ones only in where their text came
    /// from, and that text says SYNTHETIC on screen so a launch with this flag can never
    /// be mistaken for a real proposal (T-54-06).</para>
    ///
    /// <para>Compiled out of Release entirely — not disabled, ABSENT — along with the
    /// flag that reaches it. See <c>App.SynthChatCards</c>.</para>
    /// </summary>
    internal void RenderSyntheticCards() => RenderCards(
    [
        new ChatOptionCard(
            "synth-card-1",
            "SYNTHETIC — trim the opener",
            "UIA test card; Apply must refuse server-side."),
        new ChatOptionCard(
            "synth-card-2",
            "SYNTHETIC — split at playhead",
            "UIA test card; Apply must refuse server-side."),
    ]);
#endif

    /// <summary>
    /// Apply one card — <c>rudis_apply_option_card</c>, this export's first caller in
    /// the C# shell.
    ///
    /// <para><b>The click carries ONLY the card id</b> (T-54-01). The backend resolves
    /// the real edit server-side from <c>AgentSession.pending_option_choice</c>
    /// (<c>app-core/src/agent_turn.rs:1708-1717</c>), so a card can only ever do what a
    /// normal tool call could already do — no new privilege surface. A stale or unknown
    /// id is REFUSED there, with <c>"unknown option card id: …"</c> or <c>"no option
    /// cards are pending"</c>, and that refusal is what the error bubble below shows
    /// verbatim.</para>
    ///
    /// <para><b>⚠ DELIBERATELY ABSENT: any snapshot or event-poll call</b>
    /// (54-RESEARCH Pitfall 3). It is tempting to "apply the result" here because the
    /// call answers with the patches it produced — but those patches were ALREADY
    /// pushed onto the standard event ring by <c>FfiAppCtx::emit_patch</c> before this
    /// returned (<c>crates/ffi/src/commands.rs:387-405</c>). The Timeline and MediaBin
    /// pick them up through the same mirror path every other mutation rides. Re-applying
    /// them here would double-apply a sequenced patch, and reaching for
    /// <c>GetSnapshotAsync</c>/<c>PollEventsAsync</c> from a region would put this file
    /// in the state-ownership business, which rule 4 gives to the backend. The returned
    /// value is used for exactly one thing: did it succeed.</para>
    /// </summary>
    private async Task ApplyCardAsync(Button apply, string cardId)
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            return;
        }

        apply.IsEnabled = false;
        try
        {
            var applied = await engine.ApplyOptionCardAsync(
                ChatCommandPayloads.ApplyOptionCard(cardId));

            if (applied.Kind == RudisResultKind.Ok)
            {
                // The choice is resolved — the offer comes off screen (v6.0's
                // clearOptionCards on the success path only).
                ClearCards();
                return;
            }

            apply.IsEnabled = true;
            AppendBubble(
                ChatBubbleKind.Error,
                applied.Error ?? $"apply_option_card failed ({applied.Kind}/{applied.Status})");
        }
        catch (Exception ex)
        {
            apply.IsEnabled = true;
            AppendBubble(ChatBubbleKind.Error, $"{ex.GetType().Name}: {ex.Message}");
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // Status + the Settings entry (Phase 69 shape; D-19 originally, main.ts:2317-2390)
    //
    // The inline key form this block used to drive was retired in Phase 69 (D-69-10):
    // Chat.KeyButton now OPENS Settings, and the status is ONE parse
    // (AgentStatusText.ParseAnthropic) feeding ONE ApplyStatus that sets the pill, the
    // key button and Chat.AgentStatus together (D-69-15 / T-69-26).
    // ════════════════════════════════════════════════════════════════════════

    /// <summary>
    /// Re-read <c>rudis_agent_status</c> and repaint the pill, the key button and
    /// <c>Chat.AgentStatus</c>. Called by <c>MainWindow</c> when the region mounts and
    /// after the Settings dialog closes.
    ///
    /// <para>ANY failure is treated as disconnected — v6.0's <c>catch</c> does the same,
    /// and it is the honest answer: if the status cannot be read, a key cannot be
    /// assumed. <c>Chat.AgentStatus</c> then reads "status could not be read" (review 69
    /// IN-01), matching Settings, rather than "no API key".</para>
    /// </summary>
    public async Task RefreshStatusAsync()
    {
        var engine = App.Engine;
        if (engine is null || engine.IsInvalid)
        {
            ApplyStatus(new AgentKeyStatus(false, KeySource.Unknown), unreadable: true);
            return;
        }

        try
        {
            var status = await engine.AgentStatusAsync();
            if (status.Kind == RudisResultKind.Ok)
            {
                ApplyStatus(AgentStatusText.ParseAnthropic(status.Value));
            }
            else
            {
                ApplyStatus(new AgentKeyStatus(false, KeySource.Unknown), unreadable: true);
            }
        }
        catch (Exception ex)
        {
            App.LogDiagnostic($"agent_status threw: {ex.GetType().Name}: {ex.Message}");
            ApplyStatus(new AgentKeyStatus(false, KeySource.Unknown), unreadable: true);
        }
    }

    /// <summary>
    /// The pill, the key button and <c>Chat.AgentStatus</c>, all from the one fact — so
    /// they can never disagree about which state is on screen.
    ///
    /// <para>The pill's UIA <c>Name</c> is v6.0's own word (<c>connected</c> /
    /// <c>disconnected</c>) while the visible TextBlock carries the uppercase
    /// presentation v6.0 got from CSS. <c>Chat.AgentStatus</c> carries the D-69-15 text
    /// (<see cref="AgentStatusText.Describe"/>), which additionally says WHERE the key came
    /// from; the pill's Name is kept as it was so prior gates do not churn.</para>
    ///
    /// <para>Disconnected uses the existing neutral tokens; the handoff calls this state
    /// amber but defines no amber token, and inventing one would break CLAUDE.md rule 7
    /// in the other direction. Reasoning in full at the pill's site in Chat.xaml.</para>
    /// </summary>
    private void ApplyStatus(AgentKeyStatus s, bool unreadable = false)
    {
        _connected = s.Configured;
        var connected = _connected;

        StatusPillText.Text = connected ? "CONNECTED" : "DISCONNECTED";
        StatusPill.Background = Token(connected ? "success-bg" : "bg-elevated");
        StatusPill.BorderBrush = Token(connected ? "success" : "border-strong");
        StatusPillText.Foreground = Token(connected ? "success" : "text-secondary");
        AutomationProperties.SetName(StatusPill, connected ? "connected" : "disconnected");

        var keyLabel = connected ? "Change key" : "Reconnect";
        KeyButton.Content = keyLabel;
        AutomationProperties.SetName(KeyButton, keyLabel);

        // Review 69 IN-01: a status that could not be READ says so (the same words Settings
        // uses) instead of claiming "no API key" — one place a status becomes words (D-69-15).
        var text = unreadable ? AgentStatusText.DescribeUnreadable() : AgentStatusText.Describe(s);
        AgentStatusLabel.Text = text;
        AutomationProperties.SetName(AgentStatusLabel, text);
        ToolTipService.SetToolTip(AgentStatusLabel, text);
    }

    /// <summary>A named design token from the app dictionary. The only way a colour
    /// enters this file — a literal here would be a build failure
    /// (<c>MechanicalGatesTests.no_raw_hex_outside_the_token_dictionary</c>).</summary>
    private static Brush Token(string key) => (Brush)Application.Current.Resources[key];

    /// <summary><c>Reconnect</c> / <c>Change key</c> opens Settings (D-69-10) — the key
    /// is entered there, never in this region.</summary>
    private void OnKeyButtonClick(object sender, RoutedEventArgs e) => RequestSettings?.Invoke();
}
