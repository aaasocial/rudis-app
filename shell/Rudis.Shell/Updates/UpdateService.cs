using System.Reflection;
using Velopack;
using Velopack.Sources;

namespace Rudis.Shell.Updates;

/// <summary>
/// The background update client (Phase 62, plan 62-02 — SHIP-03).
///
/// <para><b>The whole contract in one sentence: this class may never be the reason a user
/// waits.</b> It is started fire-and-forget AFTER the main window is activated, it does its
/// work on a thread-pool thread, it shows no dialog, it retries nothing, and every failure
/// path is a logged return. CLAUDE.md rule 5 (offline core) is not "the updater degrades
/// gracefully offline" — it is "editing and exporting never touch the network", and the only
/// way to keep that true is for this class to be structurally incapable of blocking
/// anything. T-62-07.</para>
///
/// <para><b>Four ways this does nothing at all</b>, in the order they are checked:</para>
/// <list type="number">
///   <item>No feed compiled in (the DEFAULT — every developer build, and every release
///     built without <c>-p:RudisUpdateFeed=</c>). Returns before constructing a source, so
///     no URL is parsed and no socket can be opened.</item>
///   <item>A feed compiled in that is not an absolute http/https URL. Refused rather than
///     handed to Velopack — see the T-62-06 note on <see cref="ResolveFeedUrl"/>.</item>
///   <item>Not a Velopack install (running from a raw <c>dotnet publish</c> folder, from
///     <c>bin\x64\Release</c>, or from a zip a user extracted). There is no install root to
///     update, so there is nothing to do. This is the arm plan 62-02's D-13 fail-first leg
///     watches: the raw publish folder REFUSES the update path where the installed app
///     engages it.</item>
///   <item>Anything at all throwing — unreachable host, DNS failure, 404, malformed feed,
///     a checksum Velopack rejects. Caught, logged, returned.</item>
/// </list>
///
/// <para><b>Apply-on-exit, never apply-now.</b> <c>WaitExitThenApplyUpdates</c> hands the
/// downloaded release to Velopack's updater and asks it to wait for THIS process to exit;
/// the swap happens after the user closes the app and the next launch is the new version.
/// A video editor must never have its binaries replaced underneath an open project, and
/// <c>restart: false</c> means the user is never bounced out of one.</para>
/// </summary>
internal static class UpdateService
{
    /// <summary>The compile-time assembly-metadata key carrying the update feed URL. Set by
    /// <c>-p:RudisUpdateFeed=…</c> through the <c>&lt;AssemblyMetadata/&gt;</c> item in
    /// Rudis.Shell.csproj. There is deliberately no environment-variable and no
    /// config-file route to the same value.</summary>
    internal const string FeedMetadataKey = "RudisUpdateFeed";

    /// <summary>Guards against a second start if a future caller ever activates twice.</summary>
    private static int _started;

    /// <summary>
    /// Kick off the background update check. Returns immediately, always.
    ///
    /// <para>Call this AFTER the main window is activated. It is not awaited, it does not
    /// touch the UI thread past this line, and it has no return value on purpose: nothing
    /// in the shell is allowed to make a decision that depends on it.</para>
    /// </summary>
    internal static void StartBackgroundCheck()
    {
        if (Interlocked.Exchange(ref _started, 1) != 0)
        {
            return;
        }

        // Gate 1 — no feed compiled in. Checked HERE, on the calling thread, and before
        // any Task is scheduled, so the default posture costs one attribute read and
        // starts no work whatsoever. There is no SimpleWebSource on this path, so there
        // is nothing that could open a socket even in principle.
        var feed = ResolveFeedUrl(typeof(UpdateService).Assembly, out var reason);
        if (feed is null)
        {
            App.LogDiagnostic($"updater: {reason}; disabled");
            return;
        }

        // Fire-and-forget BY CONSTRUCTION. Never `await`, never `.Result`, never
        // `.GetAwaiter().GetResult()`. This is a rule rather than a preference because
        // plan 62-02 watched the blocking variant of this exact line do the damage
        // (RED-62-02-T1): against a black-holed feed the shell emitted one diagnostic and
        // then went silent — no ABI probe, no preview attach, no further line for 15 s —
        // while BOTH naive liveness checks (a non-zero MainWindowHandle at 1665 ms, and
        // Process.Responding at 1681 ms) cheerfully reported a healthy startup. A
        // regression here would be invisible to anything except the shell's own voice.
        _ = Task.Run(() => CheckAsync(feed));
    }

    /// <summary>
    /// Read the feed URL that was baked in at compile time, or null when there is none.
    ///
    /// <para><b>T-62-06.</b> The URL comes from <see cref="AssemblyMetadataAttribute"/> and
    /// from nowhere else. An env var or a <c>.json</c> beside the exe would let anything
    /// running as the user re-point the app's code-download channel by writing a file —
    /// strictly worse than the baseline, where an attacker with that privilege has to
    /// replace the binary itself (noisier, and something a signature check can catch).
    /// Changing the feed here requires a rebuild, which 62-04 will require a signature on.</para>
    ///
    /// <para>Non-absolute and non-http(s) values are REFUSED rather than forwarded. A
    /// relative URI, a <c>file://</c> path or a <c>ftp://</c> URL reaching an update source
    /// would each be a different and unreviewed trust boundary; the release script is the
    /// place that gets to choose the feed, and the only shape it may choose is a web one.
    /// 62-04's runbook narrows this further to https-only.</para>
    ///
    /// <para><paramref name="reason"/> carries WHY, and the two whys are deliberately
    /// different sentences. An earlier draft logged "no feed configured" on both paths;
    /// a launch transcript against a <c>file://</c> feed then read "…is not an absolute
    /// http/https URL" IMMEDIATELY followed by "no feed configured", which is a diagnostic
    /// contradicting itself about the one fact an incident would turn on — whether the
    /// build shipped a feed at all. Caught by running the case rather than by review.</para>
    /// </summary>
    internal static string? ResolveFeedUrl(Assembly assembly, out string reason)
    {
        string? raw = null;
        foreach (var meta in assembly.GetCustomAttributes<AssemblyMetadataAttribute>())
        {
            if (string.Equals(meta.Key, FeedMetadataKey, StringComparison.Ordinal))
            {
                raw = meta.Value;
                break;
            }
        }

        if (string.IsNullOrWhiteSpace(raw))
        {
            reason = "no feed configured";
            return null;
        }

        raw = raw.Trim();
        if (!Uri.TryCreate(raw, UriKind.Absolute, out var uri)
            || (uri.Scheme != Uri.UriSchemeHttp && uri.Scheme != Uri.UriSchemeHttps))
        {
            // The rejected value is NOT echoed. It is attacker-influenced only in the
            // sense that whoever built this binary chose it, but the log-safe rule here
            // is "names, never values", and a URL is a value.
            reason = "compiled-in feed is not an absolute http/https URL";
            return null;
        }

        reason = string.Empty;
        return raw;
    }

    /// <summary>
    /// The background body. Every exit is a log line; nothing here throws to the caller
    /// (there is no caller — it runs on the thread pool).
    /// </summary>
    private static async Task CheckAsync(string feedUrl)
    {
        try
        {
            var manager = new UpdateManager(new SimpleWebSource(feedUrl));

            // Gate 3 — not installed. Constructing the manager and the source performs no
            // I/O (SimpleWebSource just holds the base URI), so reaching this line has
            // still opened no connection. A raw publish folder stops here.
            if (!manager.IsInstalled)
            {
                App.LogDiagnostic("updater: not a velopack install; skipping");
                return;
            }

            var info = await manager.CheckForUpdatesAsync().ConfigureAwait(false);
            if (info is null)
            {
                App.LogDiagnostic("updater: up to date");
                return;
            }

            await manager.DownloadUpdatesAsync(info).ConfigureAwait(false);

            // Apply AFTER the user exits. `silent: true` — no Velopack progress window; the
            // user asked to close the app, not to watch an installer. `restart: false` — a
            // desktop editor that relaunches itself uninvited is a bug, not a feature.
            manager.WaitExitThenApplyUpdates(info.TargetFullRelease, silent: true, restart: false);

            // ⚠ 62-03's delta-measurement harness keys off this EXACT sentence. Changing
            // its wording silently breaks a downstream measurement rather than a build.
            App.LogDiagnostic(
                $"updater: downloaded {info.TargetFullRelease.Version}; will apply on exit");
        }
        catch (Exception ex)
        {
            // Gate 4 — everything else. Offline, DNS failure, connection refused, a 404 on
            // the feed, a corrupt manifest, a checksum Velopack refused. All the same
            // answer: say so once, and get out of the user's way.
            //
            // Log-safe by construction (App.LogDiagnostic's standing rule): the exception
            // TYPE and MESSAGE only. Velopack's messages name the feed URL, which is a
            // compile-time constant of this build and carries no user data.
            App.LogDiagnostic($"updater: check failed ({ex.GetType().Name}: {ex.Message}); ignored");
        }
    }
}
