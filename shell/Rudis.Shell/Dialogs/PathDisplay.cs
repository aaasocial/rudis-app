namespace Rudis.Shell.Dialogs;

/// <summary>
/// Human-readable rendering of Windows extended-length ("verbatim") paths.
///
/// <para>Debug session <c>export-no-file-written</c> (2026-08-01): imported media
/// paths are canonicalized by the Rust backend with <c>std::fs::canonicalize</c>,
/// which on Windows yields the <c>\\?\C:\...</c> verbatim form. That form is
/// CORRECT internally (the engine's ffmpeg sidecar handles it — proven by a live
/// export to a verbatim out_path during the investigation), but it is
/// user-hostile on screen: the ExportDialog's default folder read as
/// <c>\\?\C:\Users\...</c>, which users do not parse as a real location. This
/// helper strips the prefix for DISPLAY ONLY; nothing internal round-trips
/// through it.</para>
/// </summary>
internal static class PathDisplay
{
    /// <summary>
    /// <c>\\?\C:\x</c> → <c>C:\x</c>, and the UNC verbatim form
    /// <c>\\?\UNC\server\share</c> → <c>\\server\share</c>. Anything else is
    /// returned unchanged.
    /// </summary>
    internal static string ToDisplay(string path)
    {
        if (path.StartsWith(@"\\?\UNC\", StringComparison.OrdinalIgnoreCase))
        {
            return @"\\" + path[@"\\?\UNC\".Length..];
        }
        if (path.StartsWith(@"\\?\", StringComparison.Ordinal))
        {
            return path[@"\\?\".Length..];
        }
        return path;
    }
}
