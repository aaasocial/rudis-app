namespace Rudis.Shell;

/// <summary>
/// A DEVELOPER CONVENIENCE: loads the repo/dist <c>.env</c> into the PROCESS environment at
/// startup, so a developer's double-clicked Desktop shortcut resolves the BYO-key variables
/// the Rust side reads (<c>ANTHROPIC_API_KEY</c>, <c>RUNWAY_API_KEY</c>,
/// <c>ELEVENLABS_API_KEY</c>, <c>RUDIS_AGENT_MODEL</c>) without exporting anything by hand.
///
/// <para><b>Where keys actually come from (Phase 69, D-69-06/07/08).</b> Precedence is
/// Windows Credential Manager (entered in Settings) → the process environment → nothing.
/// <c>.env</c> is NOT a resolution tier: it only pre-fills the process environment for
/// variables not already set. The shipped app never carries a <c>.env</c> — the
/// installer/Velopack pack asserts its absence (<c>scripts/release/build-release.ps1</c>) —
/// and no key is ever migrated from <c>.env</c> or the environment into Credential Manager
/// (D-69-08). The Chat/Settings status line says which tier a key came from.</para>
///
/// <para><b>Opt-out.</b> <c>RUDIS_NO_DOTENV=1</c> skips the load entirely, before any file
/// is looked for. It ships in Release on purpose: an opt-out can only REDUCE trust (it
/// skips reading a plaintext secrets file), and the UIA harness needs it to prove the
/// "no keys anywhere" state without touching the owner's real <c>.env</c>.</para>
///
/// <para><b>Why this lives in the SHELL and not in <c>rudis_init</c>.</b> The obvious
/// alternative — a <c>dotenvy::dotenv()</c> inside <c>crates/ffi</c>'s init export, which is
/// where the retired Tauri shell's <c>run()</c> used to do it — would arm real, billable
/// credentials inside test runs: the contract tier calls <c>rudis_init</c> directly, and
/// <c>agent-gen/src/runway.rs:3661</c> and <c>elevenlabs.rs:444</c> both branch on the
/// documented premise that "<c>cargo test</c> does not load <c>.env</c>". Keeping the load
/// on the managed side preserves that premise exactly.</para>
///
/// <para><b>Mechanism.</b> <see cref="Environment.SetEnvironmentVariable(string,string)"/>
/// with the process target writes the real Win32 environment block on Windows, which Rust's
/// <c>std::env::var</c> reads back through <c>GetEnvironmentVariableW</c> — the same
/// cross-language hand-off <c>Rudis.Shell.EvalHarness.EnvFile.ResolveAndInstall</c> already
/// depends on. <c>EnvBootstrapTests</c> pins it against kernel32 rather than assuming it.</para>
///
/// <para><b>Secrecy.</b> A parsed VALUE is never logged, never returned to a caller and never
/// put in an exception message. The diagnostic line names the file and the variable NAMES
/// only — enough to debug "why is my key not loading", useless to anyone reading a log.</para>
/// </summary>
internal static class EnvBootstrap
{
    /// <summary>The marker that identifies the repo root when walking up from a dev build
    /// output — the same marker <c>EvalHarness.EnvFile.FindRepoRoot</c> uses. A tracked
    /// file, not the planning tree: the public repository ships without <c>.planning/</c>.</summary>
    private const string RepoMarker = "shell/Rudis.Shell/Rudis.Shell.csproj";

    /// <summary>Phase 69 (D-69-07). <c>RUDIS_NO_DOTENV=1</c> skips <see cref="Load"/>
    /// entirely. Release-included by design — see the class summary.</summary>
    internal const string NoDotenvEnvName = "RUDIS_NO_DOTENV";

    /// <summary>What <see cref="Load"/> did. Carries no key material — only the file that
    /// served and the NAMES involved, so a caller can log it safely.</summary>
    internal readonly record struct Result(
        string? Path,
        IReadOnlyList<string> Applied,
        IReadOnlyList<string> SkippedAlreadySet,
        bool OptedOut = false)
    {
        /// <summary>A single log-safe line. Values never appear.</summary>
        internal string Describe() => OptedOut
            ? "env: .env loading disabled by RUDIS_NO_DOTENV=1 — relying on Windows Credential Manager and the ambient environment"
            : Path is null
            ? "env: no .env found — developer convenience only; the app reads Windows Credential Manager first, then the ambient environment"
            : $"env: loaded {Path} (applied: {(Applied.Count == 0 ? "none" : string.Join(", ", Applied))}"
              + (SkippedAlreadySet.Count == 0
                  ? ")"
                  : $"; already set, left alone: {string.Join(", ", SkippedAlreadySet)})");
    }

    /// <summary>
    /// Find the first <c>.env</c> and install its variables into the process environment.
    /// Never throws: a missing or unreadable file is a normal state (the app runs fine
    /// without keys — only the AI/generation features degrade), so failure is reported
    /// through the returned <see cref="Result"/>, not an exception.
    /// </summary>
    internal static Result Load()
    {
        if (Environment.GetEnvironmentVariable(NoDotenvEnvName) == "1")
        {
            return new Result(null, Array.Empty<string>(), Array.Empty<string>(), OptedOut: true);
        }

        var path = FindEnvFile();
        if (path is null)
        {
            return new Result(null, Array.Empty<string>(), Array.Empty<string>());
        }

        string[] lines;
        try
        {
            lines = File.ReadAllLines(path);
        }
        catch (IOException)
        {
            return new Result(null, Array.Empty<string>(), Array.Empty<string>());
        }
        catch (UnauthorizedAccessException)
        {
            return new Result(null, Array.Empty<string>(), Array.Empty<string>());
        }

        var applied = new List<string>();
        var skipped = new List<string>();

        foreach (var (name, value) in Parse(lines))
        {
            // dotenv semantics, matching dotenvy and EnvFile.ResolveAndInstall: a variable
            // the user (or a parent process) exported explicitly ALWAYS beats the file.
            if (!string.IsNullOrEmpty(Environment.GetEnvironmentVariable(name)))
            {
                skipped.Add(name);
                continue;
            }

            Environment.SetEnvironmentVariable(name, value);
            applied.Add(name);
        }

        return new Result(path, applied, skipped);
    }

    /// <summary>
    /// The first <c>.env</c> of two candidates, or null when neither exists:
    /// <list type="number">
    /// <item>Beside the running executable — a DEVELOPER CONVENIENCE, never the shipped
    /// mechanism. It exists only because <c>install-desktop-shortcut.ps1</c> hardlinks the
    /// repo file into the developer's published <c>dist/</c> dir (one inode, not two divergent
    /// copies of the same secrets). A real install never has one: the release pack asserts
    /// no <c>.env</c> is present, and keys ship via Settings → Windows Credential Manager.</item>
    /// <item>The <c>.env</c> of the nearest ancestor directory containing <c>.planning/</c>
    /// — THE DEV PATH, so running straight out of <c>bin\x64\Debug\...</c> picks up the repo
    /// file with no install step.</item>
    /// </list>
    /// </summary>
    private static string? FindEnvFile()
    {
        var beside = Path.Combine(AppContext.BaseDirectory, ".env");
        if (File.Exists(beside))
        {
            return beside;
        }

        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null)
        {
            if (File.Exists(Path.Combine(dir.FullName, RepoMarker)))
            {
                var repo = Path.Combine(dir.FullName, ".env");
                return File.Exists(repo) ? repo : null;
            }
            dir = dir.Parent;
        }

        return null;
    }

    /// <summary>
    /// <c>KEY=VALUE</c> lines, blanks and <c>#</c> comments ignored. Byte-equivalent to the
    /// rules in <c>EvalHarness/EnvFile.cs:47-68</c> — deliberately, so the shipping app and
    /// the eval gate can never disagree about what a given <c>.env</c> means.
    ///
    /// <para>Splits on the FIRST <c>=</c>: a key's value may itself contain <c>=</c>, which
    /// base64-ish provider tokens routinely do.</para>
    /// </summary>
    internal static IEnumerable<(string Name, string Value)> Parse(IEnumerable<string> lines)
    {
        foreach (var raw in lines)
        {
            var line = raw.Trim();
            if (line.Length == 0 || line.StartsWith('#'))
            {
                continue;
            }

            var eq = line.IndexOf('=');
            if (eq <= 0)
            {
                continue;
            }

            var name = line[..eq].Trim();
            var value = line[(eq + 1)..].Trim();

            // Strip one layer of matching quotes, the usual .env convention.
            if (value.Length >= 2 &&
                ((value[0] == '"' && value[^1] == '"') || (value[0] == '\'' && value[^1] == '\'')))
            {
                value = value[1..^1];
            }

            if (name.Length == 0)
            {
                continue;
            }

            yield return (name, value);
        }
    }
}
