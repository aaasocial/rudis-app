using System.Globalization;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;

namespace Rudis.Shell.Regions;

/// <summary>
/// One row of <c>Toolbar.Project.Recent</c>, projected from a single
/// <c>rudis_get_projects</c> element.
///
/// <para>Every field is read BY NAME from the payload, never by position. Plan 60.1-04
/// measured — and asserted — that the engine serialises these keys ALPHABETICALLY
/// (<c>isActive</c>, <c>modifiedUnixMs</c>, <c>name</c>, <c>path</c>), because serde's
/// map is a <c>BTreeMap</c> here. That is a fact about the producer, not a promise; a
/// reader that depended on it would break silently the first time a field was
/// renamed.</para>
///
/// <para><b><see cref="Name"/> is the UNTOUCHED string, and <see cref="DisplayName"/>
/// is the safe one.</b> The distinction is load-bearing in both directions: the name
/// route addresses a project BY this exact string, so a capped copy would address
/// nothing; and the menu renders <see cref="DisplayName"/>, because the raw one came
/// out of a file (T-60.1-06).</para>
/// </summary>
internal sealed record RecentProjectRow(string Name, string Path, bool IsActive, long ModifiedUnixMs)
{
    /// <summary>The capped, control-character-free string a menu row renders.</summary>
    internal string DisplayName => ToolbarProjectRoutes.CapForDisplay(Name);

    /// <summary>
    /// The last-modified stamp, in the user's own locale. Empty when the engine
    /// reported no usable stamp, so a row never renders the 1970 epoch as if it were
    /// a real date.
    /// </summary>
    internal string DisplayModified => ModifiedUnixMs <= 0
        ? string.Empty
        : DateTimeOffset.FromUnixTimeMilliseconds(ModifiedUnixMs)
            .ToLocalTime()
            .ToString("g", CultureInfo.CurrentCulture);
}

/// <summary>
/// The <c>Toolbar</c> region's project-lifecycle DECISION half — argument
/// marshalling, the Open Recent projection, and the two Save As target rules —
/// deliberately free of every WinUI type.
///
/// <para><b>Why it is a file and not a member of <c>Toolbar.xaml.cs</c>.</b> Plan
/// 60.1-05 asked for <c>BuildPathArgs</c> "so it is reachable without XAML". Inside
/// the region partial it would not have been: <c>Rudis.Shell.Tests</c> compiles the
/// production sources DIRECTLY (same files, not copies) and deliberately never lists
/// the WinUI-bearing region files, so a member there is unreachable from a test
/// whatever its accessibility says. <c>PreviewMonitorCommand.cs</c> (plan 52-13) is
/// the same move for the same reason: the payload BUILDER is pure, the region that
/// calls it is not, and only the builder is linked into the test tier.</para>
///
/// <para><b>Everything here is a DISPLAY or MARSHALLING rule. None of it is
/// validation.</b> <c>sanitize_project_name</c> owns what a project may be called
/// (T-26-01, including the Windows reserved-device-name trap a from-scratch validator
/// misses), and <c>RudProjectPath</c> / <c>RudSaveTargetPath</c> own what a path may
/// be. A second copy of either rule in the shell is precisely the drift the
/// don't-hand-roll rule exists to prevent, so the shell asks and shows the
/// answer.</para>
/// </summary>
internal static partial class ToolbarProjectRoutes
{
    /// <summary>
    /// The longest project name a menu row will render (T-60.1-06).
    ///
    /// <para><c>scan_known_projects</c> reads <c>Project.name</c> out of each file, so
    /// anything with write access to the projects directory chooses that string.
    /// <c>MenuFlyoutItem.Text</c> is inert — this is a LAYOUT concern, not an
    /// injection one — but an unbounded name turns the only surface from which a user
    /// can reach their other projects into a menu they cannot use.</para>
    /// </summary>
    internal const int RecentNameDisplayCap = 48;

    /// <summary>The one extension a Rudis project file may have. The backend's
    /// <c>RudSaveTargetPath</c> enforces it; this constant only makes the shell stop
    /// offering a target that would be refused.</summary>
    internal const string RudExtension = ".rud";

    /// <summary>Serde's wrapper around a rejected argument struct.</summary>
    private const string InvalidArgumentsPrefix = "invalid arguments: ";

    /// <summary>Shown when the engine refuses with nothing readable attached. Never
    /// empty: an empty refusal is a silent one, which is the exact failure UI-SPEC
    /// section 5 forbids.</summary>
    private const string FallbackRefusal =
        "The engine refused, and gave no reason. The raw answer is in the diagnostic log.";

    /// <summary>
    /// <c>{"path": ".."}</c> for <c>rudis_open_project_at_path</c> and
    /// <c>rudis_save_project_as</c>.
    ///
    /// <para><b>Built with <see cref="JsonObject"/>, never by interpolation</b>
    /// (T-52-34). A Windows path is the single value most likely to break a hand-built
    /// JSON string — every separator is a JSON escape character, and the canonical
    /// form both of those exports hand back opens with FOUR of them. Plan 60.1-04
    /// shipped the wrappers with no <c>string path</c> overload on purpose, so that
    /// this is the only way to build the argument and there is nowhere for a second
    /// style to drift to.</para>
    ///
    /// <para>The caller runs <c>Path.GetFullPath</c> first (T-51-22). This routine
    /// does NOT canonicalise: a path is canonicalised once, where it is chosen.</para>
    /// </summary>
    internal static string BuildPathArgs(string path) =>
        new JsonObject { ["path"] = path }.ToJsonString();

    /// <summary>
    /// <c>{"name": ".."}</c> for <c>rudis_new_project</c> and
    /// <c>rudis_open_project</c>. Same rule, same reason: whatever the user typed
    /// reaches the engine intact, INCLUDING the characters that would end a naively
    /// built JSON string early — because deciding what a name may contain is
    /// <c>sanitize_project_name</c>'s job, not the shell's.
    /// </summary>
    internal static string BuildNameArgs(string name) =>
        new JsonObject { ["name"] = name }.ToJsonString();

    /// <summary>
    /// Project the <c>rudis_get_projects</c> array into ordered menu rows:
    /// most-recently-modified first, ties broken by name so the order is stable
    /// between two renders of an unchanged directory.
    ///
    /// <para><b>Total by construction.</b> It runs inside a <c>MenuFlyout.Opening</c>
    /// handler, where an exception takes the whole menu with it, so every shape other
    /// than the contract's — a prose string (which is what the two sibling exports
    /// answer), an undefined element from a transport fault, a row missing a field —
    /// yields FEWER rows rather than a throw. It never invents one.</para>
    ///
    /// <para><b><c>isActive</c> is reported exactly as the engine reports it.</b>
    /// <c>D-60.1-04</c> is live: <c>run_get_projects_detailed</c> compares a canonical
    /// extended-length path against a plain scan-form one, so a project opened BY PATH
    /// reads <c>false</c> even when its file sits inside the managed projects
    /// directory. Reconciling that here would hide a Rust defect from the Rust tests
    /// that own it, and would change this flag's meaning from "what the engine knows"
    /// to "what the shell guessed". The fix belongs in
    /// <c>run_get_projects_detailed</c>, canonicalising both sides.</para>
    /// </summary>
    internal static IReadOnlyList<RecentProjectRow> ProjectRows(JsonElement payload)
    {
        if (payload.ValueKind != JsonValueKind.Array)
        {
            return [];
        }

        var rows = new List<RecentProjectRow>();
        foreach (var element in payload.EnumerateArray())
        {
            if (element.ValueKind != JsonValueKind.Object)
            {
                continue;
            }

            if (!element.TryGetProperty("name", out var nameProperty)
                || nameProperty.ValueKind != JsonValueKind.String)
            {
                continue;
            }

            var name = nameProperty.GetString();
            if (string.IsNullOrEmpty(name))
            {
                continue;
            }

            var path = element.TryGetProperty("path", out var pathProperty)
                       && pathProperty.ValueKind == JsonValueKind.String
                ? pathProperty.GetString() ?? string.Empty
                : string.Empty;

            var isActive = element.TryGetProperty("isActive", out var activeProperty)
                           && activeProperty.ValueKind == JsonValueKind.True;

            var modified = element.TryGetProperty("modifiedUnixMs", out var modifiedProperty)
                           && modifiedProperty.ValueKind == JsonValueKind.Number
                           && modifiedProperty.TryGetInt64(out var milliseconds)
                ? milliseconds
                : 0L;

            rows.Add(new RecentProjectRow(name, path, isActive, modified));
        }

        rows.Sort(static (left, right) =>
        {
            var byRecency = right.ModifiedUnixMs.CompareTo(left.ModifiedUnixMs);
            return byRecency != 0 ? byRecency : string.CompareOrdinal(left.Name, right.Name);
        });

        return rows;
    }

    /// <summary>
    /// The display form of a name that came out of a file: control characters flattened
    /// to spaces, then capped at <see cref="RecentNameDisplayCap"/> with an ellipsis.
    ///
    /// <para>Both halves matter and they guard different things. The cap stops one long
    /// name from making the menu unreadable; the flatten stops one embedded newline
    /// from making a single row as tall as the window. Neither is a security boundary
    /// — the text is inert — and neither touches
    /// <see cref="RecentProjectRow.Name"/>, which is what the name route sends back.</para>
    /// </summary>
    internal static string CapForDisplay(string name)
    {
        if (string.IsNullOrEmpty(name))
        {
            return string.Empty;
        }

        var flattened = new StringBuilder(name.Length);
        foreach (var character in name)
        {
            flattened.Append(char.IsControl(character) ? ' ' : character);
        }

        var flat = flattened.ToString();
        return flat.Length <= RecentNameDisplayCap
            ? flat
            : string.Concat(flat.AsSpan(0, RecentNameDisplayCap - 1), "…");
    }

    /// <summary>
    /// Append <c>.rud</c> unless the name already ends in it, in any casing.
    ///
    /// <para>A DIFFERENT extension is kept and <c>.rud</c> goes after it
    /// (<c>clip.mp4</c> becomes <c>clip.mp4.rud</c>). <c>ExportDialog</c> does the
    /// opposite — it DROPS whatever the user typed — and the difference is deliberate:
    /// there the extension selects a container and a stale one would name the wrong
    /// muxer, whereas here <c>.rud</c> is the only possibility and rewriting the rest
    /// of the user's name is the silent sanitising this plan refuses to do
    /// anywhere else.</para>
    /// </summary>
    internal static string EnsureRudExtension(string fileName)
    {
        var trimmed = fileName?.Trim() ?? string.Empty;
        return trimmed.EndsWith(RudExtension, StringComparison.OrdinalIgnoreCase)
            ? trimmed
            : trimmed + RudExtension;
    }

    /// <summary>
    /// The Save As target-name guard (T-51-22): the reason a name is unusable, or
    /// <c>null</c> when it is fine.
    ///
    /// <para><b>Refused, not sanitised</b> — <c>ExportChoice.OutPath</c>'s recorded
    /// reasoning, one surface over: a path separator smuggled into a file name writes
    /// OUTSIDE the folder the user chose, and a silently-rewritten name puts the file
    /// somewhere they did not ask for and then reports success. The message says which
    /// character and why, because a refusal a user cannot act on is barely better than
    /// a silent one.</para>
    /// </summary>
    internal static string? RefuseSaveTargetName(string fileName)
    {
        var name = fileName?.Trim() ?? string.Empty;
        if (name.Length == 0)
        {
            return "Enter a name for the project file.";
        }

        var invalid = name.IndexOfAny(Path.GetInvalidFileNameChars());
        if (invalid >= 0)
        {
            var offender = char.IsControl(name[invalid])
                ? "that control character"
                : $"'{name[invalid]}'";
            return $"{offender} cannot appear in a project file name. A path separator here would " +
                   "write outside the folder you chose, so it is refused rather than sanitised.";
        }

        return null;
    }

    /// <summary>
    /// A domain refusal, trimmed for a human (<c>D-60.1-05</c>).
    ///
    /// <para>The three refusals plan 60.1-04 quoted verbatim all arrive wrapped: serde
    /// puts <c>invalid arguments: </c> in front and <c> at line 1 column N</c> behind,
    /// and the sentence a beginner needs is in the middle. Both wrappers are noise on
    /// screen and information in a log, so this trims for DISPLAY only — every caller
    /// logs the raw string as well.</para>
    ///
    /// <para>It never returns empty. If trimming would leave nothing, the raw string
    /// comes back instead, because a refusal with no words on it is indistinguishable
    /// from no refusal.</para>
    /// </summary>
    internal static string RefusalForDisplay(string? engineError)
    {
        var raw = engineError?.Trim() ?? string.Empty;
        if (raw.Length == 0)
        {
            return FallbackRefusal;
        }

        var trimmed = raw.StartsWith(InvalidArgumentsPrefix, StringComparison.Ordinal)
            ? raw[InvalidArgumentsPrefix.Length..]
            : raw;

        trimmed = SerdePositionSuffix().Replace(trimmed, string.Empty).Trim();
        return trimmed.Length == 0 ? raw : trimmed;
    }

    /// <summary>serde_json's trailing position marker, e.g. <c> at line 1 column 101</c>.</summary>
    [GeneratedRegex(@"\s+at line \d+ column \d+\s*$")]
    private static partial Regex SerdePositionSuffix();
}
