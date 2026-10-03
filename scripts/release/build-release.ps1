<#
.SYNOPSIS
  One command from this working tree to a real, installable Rudis release:
  dotnet publish -> vpk pack -> dist/velopack/.

.DESCRIPTION
  Phase 62, plan 62-02 (SHIP-02 / SHIP-03). Until this script existed the project
  had NO installer at all -- `dotnet publish` emitted an unpackaged, self-contained
  folder and that was the whole distribution story. (The WiX artifact under target/
  is a pre-cutover Tauri leftover, not a live decision.)

  WHAT THIS DOES NOT DO. It does not repackage the app. Velopack WRAPS the existing
  publish output, so D-02 (WindowsPackageType=None + SelfContained=true) is not
  reopened and MSIX is not adopted -- research/v9/STACK.md Q5 rejects MSIX
  explicitly, because its only benefit is Microsoft Store eligibility and its cost
  is reversing a considered architectural decision.

  THE GUARD THAT RUNS FIRST. Before anything is built, the shipped FFmpeg sidecar
  is re-checked for the three patent-encumbered software encoders SHIP-04 removed
  (libopenh264 / libkvazaar / libvvenc), and for the PRESENCE of h264_mf, which is
  the product's DEFAULT_VIDEO_ENCODER. scripts/windows/fetch-lgpl-ffmpeg.ps1
  already enforces this at fetch time; it is enforced AGAIN here because a release
  is the last place the payload can still be wrong, and an installer that shipped a
  contaminated ffmpeg.exe would silently undo the licensing work of plan 62-01
  while looking like a successful build.

  SIGNING (SHIP-01, filled by plan 62-04). -Sign composes the Azure Trusted
  Signing signtool template from three environment variables and hands it to vpk;
  -SignTemplate remains a raw pass-through for anything else (the rotation
  rehearsal uses it with a throwaway store-resident certificate addressed by
  thumbprint). Neither is a default: with NEITHER flag
  this script prints an unmissable UNSIGNED banner, because an unsigned installer
  will trip SmartScreen and SHIP-01 is not claimable from it.

  THE CERTIFICATE IS AN OWNER ACTION AND DOES NOT EXIST YET. -Sign THROWS, with
  setup instructions, if RUDIS_SIGN_ENDPOINT / RUDIS_SIGN_ACCOUNT /
  RUDIS_SIGN_PROFILE are not all set. It never degrades to an unsigned build
  while looking like a signed one - D-05's whole point is that a silent pass here
  is worse than a loud refusal.

  PHASE 72 RESCOPE (2026-10-02, 72-CONTEXT D-01/D-02). SHIP-01 is now an UNSIGNED GitHub
  Release with a published SHA256SUMS.txt. -Sign and -SignTemplate stay in this file as
  an unused seam: not wired into the release path, not removed. The LAST step of this
  script writes SHA256SUMS.txt for $ChecksumAssets (write-release-checksums.ps1) and
  verifies it in place (verify-release-checksums.ps1) as the pre-upload gate; a non-zero
  exit from either fails the build.

  NOTHING ABOUT THE SIGNING CONFIG IS EVER PRINTED (T-62-14). The three values go
  into a metadata.json written to a freshly-created, ACL-restricted directory
  under %TEMP%, which is zero-filled and deleted in a finally block on every exit
  path. The vpk command line echoed to the transcript has its --signTemplate and
  --signParams values REDACTED - not because today's template carries a secret,
  but because the obvious next thing anyone reaches for (`signtool /f cert.pfx /p
  <password>`) does, and a transcript is exactly where it would end up.

  NOTE: pure ASCII only -- PowerShell 5.1 reads a UTF-8 (no BOM) .ps1 as ANSI, so
  non-ASCII punctuation (em-dashes, curly quotes) corrupts parsing. Keep it ASCII.

.PARAMETER Version
  SemVer for this release, e.g. 62.0.1. Flows to -p:Version= (so the shell's
  FileVersionInfo.ProductVersion carries it) AND to vpk's --packVersion. Required:
  there is deliberately no default, because a release with an accidental version is
  a release the updater will make the wrong decision about.

.PARAMETER FeedUrl
  The update feed URL to BAKE INTO the binary at compile time
  (-p:RudisUpdateFeed=). Optional. Omitted -> the shipped updater is a silent
  no-op and the app performs zero network work (CLAUDE.md rule 5). There is
  deliberately no environment-variable or config-file route to this value in the
  shipped binary -- see threat T-62-06 and Updates/UpdateService.cs.

.PARAMETER Sign
  Sign every Rudis-built binary and the installer with Azure Trusted Signing.
  Requires RUDIS_SIGN_ENDPOINT, RUDIS_SIGN_ACCOUNT and RUDIS_SIGN_PROFILE (see
  docs/RELEASE-RUNBOOK.md section C). Throws if any is missing.

.PARAMETER SignTemplate
  A raw custom signing command passed to vpk --signTemplate; {{file}} is
  substituted by vpk with ONE path per invocation (measured - see
  62-04-vpk-sign-seam-probe.md FINDING 2). Mutually exclusive with -Sign. Used by
  the certificate-rotation rehearsal. NOTE: unlike -Sign, this does NOT apply
  $DefaultSignExclude unless -SignExclude is also passed.

.PARAMETER SignExclude
  A regex of paths to keep OUT of the signed set, passed to vpk --signExclude.
  The default excludes the redistributed third-party payload; see the comment on
  $DefaultSignExclude below for why, and note the no-backslashes rule.

.PARAMETER OutDir
  Where vpk writes the Setup exe, nupkg(s) and release manifest.
  Default: dist/velopack (gitignored). Deliberately NOT the pre-existing dist/
  root, which is a pre-cutover leftover holding the only surviving copy of the
  pre-SHIP-04 ffmpeg payload -- see PROVENANCE.md Entry 5.

.PARAMETER SkipPublish
  Reuse an existing publish directory instead of rebuilding it. For iterating on
  the pack step only; a real release never uses this.

.PARAMETER ChecksumAssets
  The user-facing assets SHA256SUMS.txt covers (Phase 72, SHIP-01 rescoped, D-02).
  Default: the Setup exe and the Portable zip -- exactly what the GitHub Release
  carries. Add the nupkg / releases.win.json only if a release uploads them.

.EXAMPLE
  ./scripts/release/build-release.ps1 -Version 10.0.0 -OutDir dist/release-10.0.0

.EXAMPLE
  ./scripts/release/build-release.ps1 -Version 62.0.1

.EXAMPLE
  ./scripts/release/build-release.ps1 -Version 62.0.2 -FeedUrl https://releases.example.com/rudis/

.EXAMPLE
  ./scripts/release/build-release.ps1 -Version 62.1.0 -FeedUrl https://releases.rudis.app/ -Sign
#>
param(
  [Parameter(Mandatory = $true)][string]$Version,
  [string]$FeedUrl = "",
  [switch]$Sign,
  [string]$SignTemplate = "",
  [string]$SignExclude = "",
  [string]$OutDir = "",
  [switch]$SkipPublish,
  [string[]]$ChecksumAssets = @('Rudis-win-Setup.exe', 'Rudis-win-Portable.zip')
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

if (-not $OutDir -or $OutDir -eq "") { $OutDir = Join-Path $repo 'dist\velopack' }

# packId is a DIFFERENT namespace from the app identifier. `app.rudis.desktop`
# names the USER-DATA dirs (%APPDATA% projects, %LOCALAPPDATA% cache) and must
# never be reused here: Velopack's install root is %LOCALAPPDATA%\{packId}, so a
# packId of app.rudis.desktop would put the install root ON TOP of the cache dir.
$PackId    = 'Rudis'
$PackTitle = 'Rudis'
$MainExe   = 'Rudis.Shell.exe'

$BannedEncoders  = @('libopenh264','libkvazaar','libvvenc')
$RequiredEncoder = 'h264_mf'

# The RFC3161 timestamp authority. NOT optional and NOT a default that can be
# dropped: PITFALLS Risk 11 turns on it. An untimestamped signature stops
# verifying the day the certificate expires, so every already-shipped build would
# acquire an expiry date; a timestamped one keeps verifying against the signing
# date forever. verify-release-signature.ps1 FAILS a file with no
# TimeStamperCertificate, so a template that quietly lost /tr is caught by the
# release gate rather than by a user in 2027.
$TimestampUrl = 'http://timestamp.acs.microsoft.com'

# WHAT DOES NOT GET SIGNED, AND WHY IT IS A DECISION RATHER THAN AN OVERSIGHT.
#
# vpk signs every payload file that is not ALREADY Authenticode-signed (measured;
# 62-04-vpk-sign-seam-probe.md FINDING 1), which by default would include the
# redistributed FFmpeg / whisper / ggml / OpenCV binaries. Signing rewrites those
# files' bytes -- and their bytes are load-bearing here. scripts/windows/fetch-lgpl-ffmpeg.ps1
# pins them by SHA256, PROVENANCE.md records those hashes, and 62-02's install
# exercise asserted that the INSTALLED binaries\ffmpeg.exe hashes equal
# runtime\binaries\ffmpeg.exe. Signing them breaks that chain: the release would
# have to choose between a signature and a checkable provenance hash.
#
# It chooses the hash. A pinned SHA256 is a stronger and more falsifiable claim
# about a redistributed binary than "we appended our signature to it", and Rudis's
# signing identity should mean "Rudis wrote this", not "Rudis carried this".
#
# NO BACKSLASHES IN THIS REGEX. Windows argument parsing collapses \\ to \ before
# vpk ever sees it, so [\\/] arrives as [\/] -- an escaped forward slash -- and
# silently matches nothing at all. That exact mistake was made and measured
# (FINDING 3). Use bare substrings for directories and [.] for a literal dot.
$DefaultSignExclude = 'binaries|(avcodec|avdevice|avfilter|avformat|avutil|swresample|swscale)-[0-9]+[.]dll$'

function Say([string]$Text) { Write-Host $Text }
function Step([string]$Text) { Write-Host ""; Write-Host ("==> " + $Text) }

function Find-EncoderRow {
  param([string[]]$Lines, [string]$Name)
  $pat = '^\s*\S+\s+' + [regex]::Escape($Name) + '(\s|$)'
  return @($Lines | Where-Object { $_ -match $pat }).Count -gt 0
}

function Format-Bytes([long]$Bytes) {
  $mb = [math]::Round($Bytes / 1MB, 1)
  return ("{0:N0} bytes ({1} MB)" -f $Bytes, $mb)
}

# ---------------------------------------------------------------------------
# SIGNING HELPERS (T-62-14: nothing here may print, persist or leak a value)
# ---------------------------------------------------------------------------

function Redact-PackArgs([string[]]$PackArgs) {
  # The echoed command line must never carry a signing command verbatim. Today's
  # Azure template holds no secret -- but `signtool /f cert.pfx /p <password>` is
  # the very next thing anyone writes here, and a build transcript is exactly
  # where a password would then live forever.
  $out = @()
  $redactNext = $false
  foreach ($a in $PackArgs) {
    if ($redactNext) { $out += '<REDACTED>'; $redactNext = $false; continue }
    $out += $a
    if ($a -eq '--signTemplate' -or $a -eq '--signParams' -or $a -eq '--azureTrustedSignFile') { $redactNext = $true }
  }
  return $out
}

function New-RestrictedDir([string]$DirPath) {
  # A directory only the current user can traverse, with inheritance from %TEMP%
  # switched OFF. The metadata file inside names the signing account; it is not a
  # private key (that never leaves Microsoft's HSM -- the decisive win of Trusted
  # Signing) but it is configuration nobody else needs to read.
  [System.IO.Directory]::CreateDirectory($DirPath) | Out-Null
  $me  = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
  $acl = New-Object System.Security.AccessControl.DirectorySecurity
  $acl.SetAccessRuleProtection($true, $false)
  $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
    $me, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')))
  Set-Acl -Path $DirPath -AclObject $acl
  return $DirPath
}

function Remove-SecretFile([string]$FilePath) {
  # Zero-fill before unlink so the contents are not left recoverable in a %TEMP%
  # slack page after an ordinary delete.
  if (-not $FilePath) { return }
  if (-not (Test-Path -LiteralPath $FilePath)) { return }
  try {
    $len = (Get-Item -LiteralPath $FilePath).Length
    [System.IO.File]::WriteAllBytes($FilePath, (New-Object byte[] $len))
  } catch { }
  Remove-Item -LiteralPath $FilePath -Force -ErrorAction SilentlyContinue
  $dir = Split-Path -Parent $FilePath
  if ($dir -and (Test-Path -LiteralPath $dir) -and
      (@(Get-ChildItem -LiteralPath $dir -Force -ErrorAction SilentlyContinue).Count -eq 0)) {
    Remove-Item -LiteralPath $dir -Force -Recurse -ErrorAction SilentlyContinue
  }
}

function Resolve-SignTool {
  if ($env:RUDIS_SIGNTOOL -and (Test-Path -LiteralPath $env:RUDIS_SIGNTOOL)) { return $env:RUDIS_SIGNTOOL }
  $roots = @(
    (Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'),
    (Join-Path $env:ProgramFiles 'Windows Kits\10\bin')
  )
  $best = $null
  foreach ($r in $roots) {
    if (-not (Test-Path -LiteralPath $r)) { continue }
    foreach ($d in (Get-ChildItem -LiteralPath $r -Directory -ErrorAction SilentlyContinue |
                    Sort-Object Name -Descending)) {
      $c = Join-Path $d.FullName 'x64\signtool.exe'
      if (Test-Path -LiteralPath $c) { $best = $c; break }
    }
    if ($best) { break }
  }
  if (-not $best) {
    throw ("signtool.exe was not found. Install the Windows SDK (Signing Tools component), or " +
           "set RUDIS_SIGNTOOL to its full path. Searched: " + ($roots -join '; '))
  }
  return $best
}

function Resolve-TrustedSigningDlib {
  # Microsoft.Trusted.Signing.Client ships Azure.CodeSigning.Dlib.dll -- the
  # signtool /dlib plugin that talks to the cloud HSM. BUILD-time only; it is
  # never part of the shipped payload (PROVENANCE.md Entry 39).
  if ($env:RUDIS_SIGN_DLIB -and (Test-Path -LiteralPath $env:RUDIS_SIGN_DLIB)) { return $env:RUDIS_SIGN_DLIB }
  $pkgRoot = Join-Path $env:USERPROFILE '.nuget\packages\microsoft.trusted.signing.client'
  if (Test-Path -LiteralPath $pkgRoot) {
    $hit = @(Get-ChildItem -LiteralPath $pkgRoot -Directory -ErrorAction SilentlyContinue |
             Sort-Object Name -Descending |
             ForEach-Object { Join-Path $_.FullName 'bin\x64\Azure.CodeSigning.Dlib.dll' } |
             Where-Object { Test-Path -LiteralPath $_ })
    if ($hit.Count -gt 0) { return $hit[0] }
  }
  throw ("Azure.CodeSigning.Dlib.dll was not found. Acquire it once with:" + [Environment]::NewLine +
         "    nuget install Microsoft.Trusted.Signing.Client -Version 1.0.60 -OutputDirectory <dir>" + [Environment]::NewLine +
         "  or   dotnet nuget ... / download from nuget.org, then set RUDIS_SIGN_DLIB to" + [Environment]::NewLine +
         "    <package>\bin\x64\Azure.CodeSigning.Dlib.dll" + [Environment]::NewLine +
         "  Searched: " + $pkgRoot + " and RUDIS_SIGN_DLIB." + [Environment]::NewLine +
         "  See docs/RELEASE-RUNBOOK.md section C.")
}

Say "======================================================================"
Say " Rudis release build"
Say ("   version   : " + $Version)
Say ("   feed      : " + $(if ($FeedUrl) { $FeedUrl } else { "(none - updater compiled in as a no-op)" }))
Say ("   signing   : " + $(if ($Sign) { "Azure Trusted Signing (-Sign)" }
                           elseif ($SignTemplate) { "custom template (-SignTemplate)" }
                           else { "NONE - this build will be UNSIGNED" }))
Say ("   outDir    : " + $OutDir)
Say ("   repo      : " + $repo)
Say "======================================================================"

# ---------------------------------------------------------------------------
# (a) CODEC HYGIENE GUARD -- runs FIRST, before anything is built.
# ---------------------------------------------------------------------------
Step "Codec hygiene guard (SHIP-04) on the payload this release will carry"

$ffmpeg = Join-Path $repo 'runtime\binaries\ffmpeg.exe'
if (-not (Test-Path -LiteralPath $ffmpeg)) {
  throw "REFUSING to build a release: $ffmpeg is missing. Run scripts/windows/fetch-lgpl-ffmpeg.ps1 first."
}

$encoders = & $ffmpeg -hide_banner -encoders 2>&1 | ForEach-Object { [string]$_ }
$banner   = (& $ffmpeg -hide_banner -version 2>&1 | Select-Object -First 1)
Say ("    payload   : " + $banner)
Say ("    sha256    : " + (Get-FileHash -LiteralPath $ffmpeg -Algorithm SHA256).Hash.ToLower())

$found = @()
foreach ($e in $BannedEncoders) { if (Find-EncoderRow -Lines $encoders -Name $e) { $found += $e } }
if ($found.Count -gt 0) {
  throw ("REFUSING to wrap a contaminated payload: patent-encumbered encoders present in " +
         "runtime/binaries/ffmpeg.exe -encoders: " + ($found -join ', ') + ". " +
         "Plan 62-01 removed these; an installer carrying them would silently undo SHIP-04. " +
         "Re-run scripts/windows/fetch-lgpl-ffmpeg.ps1 against the pinned asset.")
}
Say ("    OK        : none of " + ($BannedEncoders -join ', ') + " is present")

if (-not (Find-EncoderRow -Lines $encoders -Name $RequiredEncoder)) {
  throw ("REFUSING to build a release: required encoder '" + $RequiredEncoder + "' is MISSING " +
         "from -encoders. That is the product's DEFAULT_VIDEO_ENCODER; a payload without it " +
         "breaks every export while looking like a licensing success.")
}
Say ("    OK        : " + $RequiredEncoder + " is present (the default video encoder)")

# ---------------------------------------------------------------------------
# (a2) SIGNING PREFLIGHT -- resolve and validate BEFORE the 15-minute publish.
#
#      Everything here can fail, and every one of those failures is a
#      configuration mistake rather than a build problem. Discovering a missing
#      environment variable after a full publish + a 738 MB pack is how a
#      release step becomes something the owner avoids running.
#
#      The metadata FILE is deliberately NOT written here. It is created inside
#      the pack step's try/finally so that its lifetime is the pack call and
#      nothing wider.
# ---------------------------------------------------------------------------
$signTool = ""
$signDlib = ""

if ($Sign -and $SignTemplate) {
  throw ("-Sign and -SignTemplate are mutually exclusive. -Sign composes the Azure Trusted " +
         "Signing template for you; -SignTemplate is the raw pass-through for anything else " +
         "(the rotation rehearsal uses it with a local PFX).")
}

if ($Sign) {
  Step "Signing preflight (SHIP-01, Azure Trusted Signing)"

  $required = @('RUDIS_SIGN_ENDPOINT', 'RUDIS_SIGN_ACCOUNT', 'RUDIS_SIGN_PROFILE')
  $absent = @()
  foreach ($v in $required) {
    $val = [Environment]::GetEnvironmentVariable($v)
    if (-not $val) { $absent += $v }
    else {
      # LENGTH ONLY. The value is never printed, here or anywhere (T-62-14).
      Say ("    " + $v.PadRight(22) + " set (" + $val.Length + " chars, not echoed)")
    }
  }
  if ($absent.Count -gt 0) {
    throw ("-Sign was requested but the signing configuration is incomplete. MISSING: " +
           ($absent -join ', ') + [Environment]::NewLine +
           [Environment]::NewLine +
           "  These are NOT secrets to invent - they identify a real Azure Trusted Signing" + [Environment]::NewLine +
           "  account, which is an OWNER ACTION that has not been taken yet:" + [Environment]::NewLine +
           [Environment]::NewLine +
           "    1. Azure portal -> Trusted Signing (Artifact Signing) -> create an account." + [Environment]::NewLine +
           "       Basic tier, ~USD 9.99/month, up to 5,000 signatures/month. A -Sign release" + [Environment]::NewLine +
           "       costs 10 signatures with the default exclude, or 54 without it (both" + [Environment]::NewLine +
           "       measured; see docs/RELEASE-RUNBOOK.md section C1). The tier is nowhere" + [Environment]::NewLine +
           "       near a constraint." + [Environment]::NewLine +
           "    2. Complete business-identity verification. Currently limited to verified" + [Environment]::NewLine +
           "       businesses / self-employed individuals in the US, Canada, EU and UK." + [Environment]::NewLine +
           "    3. Create a certificate profile." + [Environment]::NewLine +
           "    4. Then set, in the release shell only:" + [Environment]::NewLine +
           "         RUDIS_SIGN_ENDPOINT  = the account's Account URI  (https://<region>.codesigning.azure.net)" + [Environment]::NewLine +
           "         RUDIS_SIGN_ACCOUNT   = the Trusted Signing account name" + [Environment]::NewLine +
           "         RUDIS_SIGN_PROFILE   = the certificate profile name" + [Environment]::NewLine +
           [Environment]::NewLine +
           "  Do NOT commit these anywhere. Full procedure: docs/RELEASE-RUNBOOK.md section C." + [Environment]::NewLine +
           "  Until then, build WITHOUT -Sign and accept the UNSIGNED banner: an unsigned" + [Environment]::NewLine +
           "  release that says so is honest; a release that silently skipped signing is not.")
  }

  $signTool = Resolve-SignTool
  Say ("    signtool              : " + $signTool)
  $signDlib = Resolve-TrustedSigningDlib
  Say ("    dlib                  : " + $signDlib)
  Say ("    timestamp (RFC3161)   : " + $TimestampUrl)
  Say  "    NOTE: no signing call has been made yet. This step only proves the"
  Say  "          configuration is complete before a long publish begins."
}

# ---------------------------------------------------------------------------
# (b) PUBLISH
# ---------------------------------------------------------------------------
$publishDir = Join-Path $repo 'shell\Rudis.Shell\bin\x64\Release\net9.0-windows10.0.26100.0\win-x64\publish'

if ($SkipPublish) {
  Step "SKIPPING publish (-SkipPublish) -- reusing the existing publish directory"
  if (-not (Test-Path -LiteralPath $publishDir)) { throw "no publish directory at $publishDir" }
} else {
  Step "dotnet publish (runs cargo, stages the runtime sidecars and compiled XAML)"
  $args = @(
    'publish', (Join-Path $repo 'shell\Rudis.Shell'),
    '-c', 'Release', '-r', 'win-x64',
    '-p:Platform=x64',
    ('-p:Version=' + $Version)
  )
  if ($FeedUrl) { $args += ('-p:RudisUpdateFeed=' + $FeedUrl) }
  Say ("    dotnet " + ($args -join ' '))
  & dotnet @args
  if ($LASTEXITCODE -ne 0) { throw "dotnet publish failed with exit code $LASTEXITCODE" }
}

# Phase 69 (OSS-02, D-69-07): the shipped payload must NEVER carry a .env. The repo .env is
# a developer convenience (hardlinked beside dist/ by install-desktop-shortcut.ps1); keys
# ship only via Windows Credential Manager through Settings.
$dotenvs = @(Get-ChildItem -LiteralPath $publishDir -Filter '.env' -Recurse -File -Force -ErrorAction SilentlyContinue)
if ($dotenvs.Count -gt 0) { throw "a .env is present in the publish directory ($($dotenvs[0].FullName)) - the shipped app must never carry one (Phase 69, OSS-02)" }
Write-Host '      no .env in the publish output (asserted)'

# ---------------------------------------------------------------------------
# (c) PUBLISH-DIRECTORY ASSERTS
#     Each class of file below is one this project has ALREADY shipped a broken
#     payload without, and each absence kills the app on start rather than at
#     build time (50-09-PACKAGING.md measured ExitCode -1073741189 and no window).
# ---------------------------------------------------------------------------
Step "Asserting the publish directory carries everything the app needs to start"

# Self-heal + tripwire for the publish-inside-publish defect plan 62-02 found and
# fixed in Rudis.Shell.csproj. StageWinUiXamlPublish used to glob $(OutDir)**\*.xbf
# without excluding $(PublishDir), which is a SUBDIRECTORY of $(OutDir) -- so every
# repeat publish copied the previous run's staged XAML one level deeper and the
# installer carried every copy. Nothing legitimately writes here, so the directory
# is garbage by definition and is removed rather than reported. Kept AFTER the fix
# so an older tree (or a tree built by an older commit) heals itself instead of
# shipping the junk, and so a regression of that Exclude is visible in a release
# transcript rather than only in a byte count. Runs BEFORE the asserts below, so
# every count they print is a count of what will actually be packed.
$nested = Join-Path $publishDir 'publish'
if (Test-Path -LiteralPath $nested) {
  $nestedBytes = (Get-ChildItem -LiteralPath $nested -Recurse -File | Measure-Object -Property Length -Sum).Sum
  Say ("    HEAL      : removing a stale nested publish\ ( " + (Format-Bytes $nestedBytes) + " ) -- see Rudis.Shell.csproj StageWinUiXamlPublish")
  Remove-Item -LiteralPath $nested -Recurse -Force
}
$required = @(
  @{ Name = 'Rudis.Shell.exe';   Why = 'the app' },
  @{ Name = 'Velopack.dll';      Why = 'the update client (SHIP-03)' },
  @{ Name = 'rudis_ffi.dll';     Why = 'the Phase 47 C ABI cdylib -- the whole engine' },
  @{ Name = 'rudis_timeline.dll';Why = 'the Timeline renderer cdylib (Phase 52)' },
  @{ Name = 'avcodec-62.dll';    Why = 'rudis_ffi.dll LOAD-TIME imports it (48-03 hwdecode)' },
  @{ Name = 'ffmpeg.exe';        Why = 'the LGPL CLI sidecar -- decode/composite/encode' },
  @{ Name = 'ffprobe.exe';       Why = 'media probing on import' },
  @{ Name = 'whisper-cli.exe';   Why = 'the transcription sidecar' },
  @{ Name = 'ggml-small.bin';    Why = 'the Whisper model (~465 MB)' },
  @{ Name = 'Rudis.Shell.pri';   Why = 'the resource index -- without it WinUI dies before any managed handler runs' }
)

$missing = @()
foreach ($r in $required) {
  $hits = @(Get-ChildItem -LiteralPath $publishDir -Filter $r.Name -Recurse -File -ErrorAction SilentlyContinue)
  if ($hits.Count -eq 0) { $missing += ($r.Name + "  (" + $r.Why + ")") }
  else { Say ("    OK  " + $r.Name.PadRight(22) + " " + (Format-Bytes $hits[0].Length)) }
}

$xbf = @(Get-ChildItem -LiteralPath $publishDir -Filter '*.xbf' -Recurse -File -ErrorAction SilentlyContinue)
if ($xbf.Count -eq 0) { $missing += "*.xbf  (compiled XAML -- StageWinUiXamlPublish did not run)" }
else { Say ("    OK  " + "*.xbf".PadRight(22) + " " + $xbf.Count + " file(s)") }

if ($missing.Count -gt 0) {
  throw ("REFUSING to pack an incomplete publish directory. Missing:`n  " + ($missing -join "`n  "))
}

$publishBytes = (Get-ChildItem -LiteralPath $publishDir -Recurse -File | Measure-Object -Property Length -Sum).Sum
$publishFiles = (Get-ChildItem -LiteralPath $publishDir -Recurse -File).Count
Say ("    TOTAL     : " + $publishFiles + " files, " + (Format-Bytes $publishBytes))

# ---------------------------------------------------------------------------
# (d) THE vpk CLI
#     A COMMITTED LOCAL tool manifest (.config/dotnet-tools.json), not a global
#     install: the version that produced a release is then reviewable in git and
#     reproducible by `dotnet tool restore`, exactly like packages.lock.json.
# ---------------------------------------------------------------------------
Step "vpk (Velopack CLI)"
Push-Location $repo
try {
  & dotnet tool restore | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "dotnet tool restore failed with exit code $LASTEXITCODE" }
  $vpkBanner = (& dotnet vpk --help 2>&1 | Select-String -Pattern 'Velopack CLI' | Select-Object -First 1)
  Say ("    " + ([string]$vpkBanner).Trim())
} finally {
  Pop-Location
}

# ---------------------------------------------------------------------------
# (e) PACK
# ---------------------------------------------------------------------------
Step "vpk pack"

if (-not (Test-Path -LiteralPath $OutDir)) { New-Item -ItemType Directory -Force -Path $OutDir | Out-Null }

if (-not $Sign -and -not $SignTemplate) {
  Write-Host ""
  Write-Host "  ################################################################" -ForegroundColor Yellow
  Write-Host "  #  UNSIGNED BUILD -- by design (Phase 72, D-01)                #" -ForegroundColor Yellow
  Write-Host "  #  Rudis ships unsigned: open source, no code-signing          #" -ForegroundColor Yellow
  Write-Host "  #  certificate, no Azure Trusted Signing. SmartScreen warns    #" -ForegroundColor Yellow
  Write-Host "  #  on first run; README.md `"Download and install`" says what    #" -ForegroundColor Yellow
  Write-Host "  #  to click. Integrity is published instead: SHA256SUMS.txt    #" -ForegroundColor Yellow
  Write-Host "  #  is written and verified as the last step below.             #" -ForegroundColor Yellow
  Write-Host "  #  verify-release-signature.ps1 REJECTS this build, and that   #" -ForegroundColor Yellow
  Write-Host "  #  is correct -- the signing seam is retained but unused       #" -ForegroundColor Yellow
  Write-Host "  #  (docs/RELEASE-RUNBOOK.md section C, marked NOT USED).       #" -ForegroundColor Yellow
  Write-Host "  #  -Sign / -SignTemplate are not part of the release path.     #" -ForegroundColor Yellow
  Write-Host "  ################################################################" -ForegroundColor Yellow
  Write-Host ""
}

# --instLocation PerUser is PINNED, not left at vpk 1.2.0's default of 'Either'.
# T-62-05: this installer must be per-user only (%LOCALAPPDATA% + HKCU) and must
# never request elevation. 'Either' lets the install land in Program Files, which
# would mean a UAC prompt and a machine-wide footprint nobody asked for.
#
# The value is 'PerUser'. It was NOT guessed: the first run of this script passed
# 'LocalAppData' (the plan's spelling) and vpk 1.2.0 rejected it by name --
#   Cannot parse argument 'LocalAppData' for option '--instLocation' as expected
#   type 'Velopack.Packaging.InstallLocation'. Did you mean one of the following?
#   Either / None / PerMachine / PerUser
# -- which is exactly why the plan said to verify flags against the installed
# tool's own help rather than trust a written spelling.
$packArgs = @(
  'vpk', '--skip-updates', 'pack',
  '--packId', $PackId,
  '--packTitle', $PackTitle,
  '--packVersion', $Version,
  '--packDir', $publishDir,
  '--mainExe', $MainExe,
  '--outputDir', $OutDir,
  '--instLocation', 'PerUser'
)

# The metadata file's whole lifetime is this try/finally, and nothing wider.
$signMetadata = ""
try {
  $effectiveTemplate = $SignTemplate

  if ($Sign) {
    $signDir = New-RestrictedDir (Join-Path $env:TEMP ('rudis-sign-' + [System.Guid]::NewGuid().ToString('N')))
    $signMetadata = Join-Path $signDir 'metadata.json'

    # Written, never echoed. This names the Trusted Signing account and profile;
    # the PRIVATE KEY does not exist on this machine at all and cannot, because
    # Trusted Signing keeps it in Microsoft's HSM. That is the single strongest
    # mitigation in this plan's threat register (T-62-14): there is no key here
    # to leak into a log, an artifact or the repository.
    $meta = [pscustomobject]@{
      Endpoint               = $env:RUDIS_SIGN_ENDPOINT
      CodeSigningAccountName = $env:RUDIS_SIGN_ACCOUNT
      CertificateProfileName = $env:RUDIS_SIGN_PROFILE
      CorrelationId          = ('rudis-' + $Version)
    }
    Set-Content -LiteralPath $signMetadata -Value ($meta | ConvertTo-Json -Depth 4) -Encoding ASCII

    # ONE FILE PER INVOCATION. vpk ignores --signParallel whenever a template is
    # used and substitutes exactly one path into {{file}} (measured, FINDING 2),
    # so this runs ~52 times per release, each one a round trip to the cloud HSM.
    # /tr + /td SHA256 is the RFC3161 timestamp Risk 11 turns on; it is not
    # optional and the verify script fails without it.
    $effectiveTemplate = ('"' + $signTool + '" sign /v /fd SHA256 /tr ' + $TimestampUrl +
                          ' /td SHA256 /dlib "' + $signDlib + '" /dmdf "' + $signMetadata + '" {{file}}')

    $excl = $(if ($SignExclude) { $SignExclude } else { $DefaultSignExclude })
    $packArgs += @('--signTemplate', $effectiveTemplate, '--signExclude', $excl)
    Say ("    signing   : Azure Trusted Signing via signtool /dlib; template REDACTED below")
    Say ("    excluding : " + $excl)
  }
  elseif ($SignTemplate) {
    $packArgs += @('--signTemplate', $SignTemplate)
    if ($SignExclude) { $packArgs += @('--signExclude', $SignExclude) }
    Say  "    signing   : custom -SignTemplate; value REDACTED below"
  }

  Push-Location $repo
  try {
    Say ("    dotnet " + ((Redact-PackArgs $packArgs) -join ' '))
    & dotnet @packArgs
    if ($LASTEXITCODE -ne 0) { throw "vpk pack failed with exit code $LASTEXITCODE" }
  } finally {
    Pop-Location
  }
}
finally {
  # Runs on success, on a vpk failure, and on Ctrl-C's terminating error alike.
  if ($signMetadata) {
    Remove-SecretFile $signMetadata
    Say ("    cleaned   : signing metadata removed (" +
         (-not (Test-Path -LiteralPath $signMetadata)) + " = gone)")
  }
}

# ---------------------------------------------------------------------------
# (f) OUTPUT ASSERTS + MEASURED SIZES
#     The ~826 MB payload is why delta updates are a REQUIREMENT and not a
#     nicety, so the real numbers get printed rather than estimated.
# ---------------------------------------------------------------------------
Step "Release artifacts"

$setup    = @(Get-ChildItem -LiteralPath $OutDir -Filter '*Setup*.exe' -File -ErrorAction SilentlyContinue)
$fullPkg  = @(Get-ChildItem -LiteralPath $OutDir -Filter '*full.nupkg' -File -ErrorAction SilentlyContinue)
$deltaPkg = @(Get-ChildItem -LiteralPath $OutDir -Filter '*delta.nupkg' -File -ErrorAction SilentlyContinue)
$manifest = @(Get-ChildItem -LiteralPath $OutDir -File -ErrorAction SilentlyContinue |
              Where-Object { $_.Name -match '^(releases.*\.json|RELEASES)$' })

$outMissing = @()
if ($setup.Count -eq 0)    { $outMissing += 'a Setup exe (*Setup*.exe)' }
if ($fullPkg.Count -eq 0)  { $outMissing += 'a full package (*full.nupkg)' }
if ($manifest.Count -eq 0) { $outMissing += 'a release manifest (releases*.json or RELEASES)' }
if ($outMissing.Count -gt 0) {
  throw ("vpk pack reported success but did not produce: " + ($outMissing -join ', ') +
         ". Contents of " + $OutDir + ": " +
         ((Get-ChildItem -LiteralPath $OutDir -File | ForEach-Object { $_.Name }) -join ', '))
}

foreach ($f in @($setup + $fullPkg + $deltaPkg + $manifest)) {
  Say ("    " + $f.Name.PadRight(34) + " " + (Format-Bytes $f.Length))
}

# ---------------------------------------------------------------------------
# (g) CHECKSUMS (SHIP-01 as rescoped by Phase 72, D-02)
#     SHA256SUMS.txt covers exactly the user-facing assets the GitHub Release
#     carries. It is written LF / ASCII / no BOM so that both `sha256sum -c`
#     and a PowerShell Get-FileHash loop accept it, then verified IN PLACE as
#     the pre-upload gate. Either script exiting non-zero fails the build.
# ---------------------------------------------------------------------------
Step "SHA256SUMS.txt (write, then verify in place)"
$sumsWriter  = Join-Path $PSScriptRoot 'write-release-checksums.ps1'
$sumsChecker = Join-Path $PSScriptRoot 'verify-release-checksums.ps1'
$sumsAssets  = ($ChecksumAssets -join ',')
$sumsDir     = (Resolve-Path -LiteralPath $OutDir).Path
& powershell -NoProfile -ExecutionPolicy Bypass -File $sumsWriter -Dir $sumsDir -Assets $sumsAssets
if ($LASTEXITCODE -ne 0) { throw "write-release-checksums.ps1 failed with exit code $LASTEXITCODE" }
& powershell -NoProfile -ExecutionPolicy Bypass -File $sumsChecker -Dir $sumsDir -Only $sumsAssets
if ($LASTEXITCODE -ne 0) { throw "verify-release-checksums.ps1 failed with exit code $LASTEXITCODE -- do NOT upload this output" }
$sumsPath = Join-Path $sumsDir 'SHA256SUMS.txt'

Say ""
Say "----------------------------------------------------------------------"
Say (" SETUP        : " + $setup[0].FullName)
Say (" FULL PACKAGE : " + (Format-Bytes $fullPkg[0].Length))
Say (" MANIFEST     : " + $manifest[0].Name)
Say (" SHA256SUMS   : " + $sumsPath)
if (-not $Sign -and -not $SignTemplate) {
  Say " SIGNED       : NO -- by design (Phase 72 D-01). Integrity = SHA256SUMS.txt; upload per docs/RELEASE-RUNBOOK.md R6-R8"
} else {
  Say " SIGNED       : signing was requested -- now PROVE it, do not assume it:"
  Say ("   powershell -File scripts/release/verify-release-signature.ps1 " + $OutDir)
}
Say "----------------------------------------------------------------------"
