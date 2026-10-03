<#
.SYNOPSIS
    Publish Rudis to dist\Rudis and drop a Desktop shortcut that launches it with keys loaded.

.DESCRIPTION
    Quick task 260821-xax. Three steps, in order, each of which exists for a reason:

    1. PUBLISH, not build. `dotnet build` stages only the ffmpeg-DEV payload (the 62/60
       DLLs rudis_ffi.dll carries load-time PE imports of). The SIDECAR payload -
       runtime\binaries' 63/61 ffmpeg.exe and ffprobe.exe - is staged only by
       StageRustFfiPublish (AfterTargets=Publish) in crates\ffi\Rudis.Ffi.targets.
       engine::resolve_binary (crates\engine\src\ffmpeg.rs:144-151) looks in the exe's own
       directory, then exe_dir\binaries, then falls through to PATH. A shortcut pointed at a
       build output would therefore run whatever ffmpeg is on PATH, which on this project's
       machines is a GPL build: a licence violation AND a silent falsifier of any
       measurement taken through it. Publish also runs StageWinUiXamlPublish, without which
       the process dies at ExitCode -1073741189 with no window and no managed exception
       (50-09-PACKAGING.md 3.3).

    2. HARDLINK the repo .env into the published directory, so EnvBootstrap.Load() finds it
       beside the executable. DEVELOPER CONVENIENCE - the shipped app reads Windows Credential
       Manager first; the .env only pre-fills the process environment for variables not
       already set (developer setup). Enter keys in Settings for the real thing. The release
       pack never carries a .env (build-release.ps1 asserts it). A hardlink, not a copy: one inode with two names means editing
       the repo .env changes what the installed app reads, and there is no second plaintext
       copy of the credentials to leak, forget, or let drift out of date. Copying is the
       fallback and warns loudly, because a stale copy of a rotated key fails in a way that
       looks like a code bug.

    3. SHORTCUT straight at the exe. No .ps1 or .vbs wrapper anywhere in the chain: nothing
       to flash a console window, nothing to trip an execution policy, and the shortcut pins
       to the taskbar and shows the app's own icon like any other Windows program.

    Re-runnable. Every step overwrites cleanly, so this doubles as the "rebuild the installed
    app after a code change" command.

.PARAMETER SkipPublish
    Reuse whatever is already in the destination. Only sane immediately after a publish.

.PARAMETER Configuration
    Build configuration. Release by default; Debug is useful when you want the app's
    DEBUG-only startup flags (--import, --place-on-timeline, ...) available from the icon.

.PARAMETER DestinationPath
    Where the app is installed. Defaults to dist\Rudis under the repo (already gitignored).

.PARAMETER ShortcutName
    Base name of the Desktop shortcut.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\windows\install-desktop-shortcut.ps1

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\windows\install-desktop-shortcut.ps1 -SkipPublish
#>
[CmdletBinding()]
param(
    [switch] $SkipPublish,
    [ValidateSet('Release', 'Debug')]
    [string] $Configuration = 'Release',
    [string] $DestinationPath,
    [string] $ShortcutName = 'Rudis'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$project  = Join-Path $repoRoot 'shell\Rudis.Shell\Rudis.Shell.csproj'

if (-not $DestinationPath) {
    $DestinationPath = Join-Path $repoRoot 'dist\Rudis'
}

if (-not (Test-Path $project)) {
    throw "cannot find the shell project at $project - run this from inside the Rudis repo."
}

# --- 1. Publish ------------------------------------------------------------------------
if ($SkipPublish) {
    Write-Host "[1/4] Skipping publish (-SkipPublish); reusing $DestinationPath"
} else {
    Write-Host "[1/4] Publishing $Configuration to $DestinationPath (this builds the Rust workspace too; first run takes a few minutes)"
    & dotnet publish $project -c $Configuration -p:Platform=x64 -o $DestinationPath --nologo
    if ($LASTEXITCODE -ne 0) {
        throw "dotnet publish failed with exit code $LASTEXITCODE."
    }
}

# --- 2. Assert the payload can actually run --------------------------------------------
# Fail here rather than shipping a shortcut to a payload that dies on launch. Each of these
# has been an observed failure: a missing .pri kills the process before any managed handler
# runs, and a missing binaries\ffmpeg.exe silently redirects the engine to PATH.
Write-Host '[2/4] Checking the published payload'

$exePath = Join-Path $DestinationPath 'Rudis.Shell.exe'
$required = @(
    @{ Path = $exePath;                                            Why = 'the app itself' },
    @{ Path = (Join-Path $DestinationPath 'rudis_ffi.dll');         Why = 'the Rust engine (C ABI)' },
    @{ Path = (Join-Path $DestinationPath 'Rudis.Shell.pri');       Why = 'the resource index; without it the process exits -1073741189 with no window' },
    @{ Path = (Join-Path $DestinationPath 'binaries\ffmpeg.exe');   Why = 'the bundled LGPL encoder sidecar; without it the engine falls through to a PATH ffmpeg' },
    @{ Path = (Join-Path $DestinationPath 'binaries\ffprobe.exe');  Why = 'the bundled LGPL probe sidecar' }
)

$missing = @()
foreach ($item in $required) {
    if (-not (Test-Path $item.Path)) {
        $missing += "  $($item.Path)`n      needed for: $($item.Why)"
    }
}
if ($missing.Count -gt 0) {
    throw "the published payload is incomplete, refusing to create a shortcut to it:`n$($missing -join "`n")"
}

# The licence check, not a nicety: the project ships closed-source and may link LGPL FFmpeg
# ONLY. A GPL binary landing here would be a licence violation shipped behind an icon.
$version = & (Join-Path $DestinationPath 'binaries\ffmpeg.exe') -version 2>&1 | Out-String
if ($version -match '--enable-gpl' -or $version -match '--enable-nonfree') {
    throw "the staged binaries\ffmpeg.exe reports a GPL/non-free configuration. Rudis ships LGPL only - check runtime\binaries and scripts\windows\fetch-lgpl-ffmpeg.ps1."
}
Write-Host '      payload complete; staged ffmpeg reports an LGPL configuration'

# --- 3. Link the .env beside the exe ---------------------------------------------------
Write-Host '[3/4] Linking .env beside the app (developer convenience)'
Write-Host '      DEVELOPER CONVENIENCE - the shipped app reads Windows Credential Manager first; the .env only pre-fills the process environment for variables not already set (developer setup). Enter keys in Settings for the real thing.'

$sourceEnv = Join-Path $repoRoot '.env'
$targetEnv = Join-Path $DestinationPath '.env'

if (-not (Test-Path $sourceEnv)) {
    Write-Warning "no .env at $sourceEnv - the app will launch, but the Chat pill will read disconnected and generation will refuse until a key is set (in-app) or a key is exported."
} else {
    if (Test-Path $targetEnv) {
        Remove-Item $targetEnv -Force
    }
    try {
        New-Item -ItemType HardLink -Path $targetEnv -Value $sourceEnv -ErrorAction Stop | Out-Null
        Write-Host '      hardlinked (one file, two names - editing the repo .env updates the installed app)'
    } catch {
        Copy-Item $sourceEnv $targetEnv -Force
        Write-Warning "could not hardlink (different volume, or a non-NTFS destination), so .env was COPIED to $targetEnv. That is a SECOND plaintext copy of your credentials: it will not track key rotations, and you should delete it when you uninstall."
    }
}

# --- 4. Desktop shortcut ----------------------------------------------------------------
Write-Host '[4/4] Creating the Desktop shortcut'

$desktop  = [Environment]::GetFolderPath([Environment+SpecialFolder]::Desktop)
$linkPath = Join-Path $desktop "$ShortcutName.lnk"

$shell = New-Object -ComObject WScript.Shell
try {
    $shortcut = $shell.CreateShortcut($linkPath)
    $shortcut.TargetPath = $exePath
    # The engine resolves its sidecars from the EXE's directory, so the working directory is
    # cosmetic for FFmpeg - but it is what a file dialog opens in, and it keeps relative
    # paths in any future CLI flag pointing somewhere sane.
    $shortcut.WorkingDirectory = $DestinationPath
    $shortcut.IconLocation = "$exePath,0"
    $shortcut.Description = 'Rudis - vibe video editing'
    $shortcut.Save()
} finally {
    [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($shell)
}

if (-not (Test-Path $linkPath)) {
    throw "the shortcut was not written to $linkPath."
}

# Explorer caches icons per source path. Because a re-run overwrites the SAME exe path, a
# changed icon would otherwise keep drawing from the cache - the shortcut looks stale and the
# obvious conclusion ("my icon change did not build") is wrong. ie4uinit rebuilds that cache.
# Best-effort: a failure here costs a stale thumbnail, never a broken install.
try {
    & "$env:SystemRoot\System32\ie4uinit.exe" -show 2>&1 | Out-Null
} catch {
    Write-Warning "could not refresh the shell icon cache; if the old icon persists, sign out and back in."
}

Write-Host ''
Write-Host "Installed. Double-click '$ShortcutName' on your Desktop."
Write-Host "  app:      $exePath"
Write-Host "  shortcut: $linkPath"
Write-Host ''
Write-Host 'After a code change, re-run this script to refresh the installed app.'
