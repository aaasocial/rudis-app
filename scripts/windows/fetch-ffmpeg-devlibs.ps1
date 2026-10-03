<#
.SYNOPSIS
  Fetch the PINNED, version-matched LGPL FFmpeg SHARED DEV build (bin/ + include/
  + import libs) into crates/engine/ffmpeg-dev/ for the in-process hwdecode path.

.DESCRIPTION
  Phase 48 promotes Phase 44's spike-local FFmpeg pin to production infrastructure
  (48-CONTEXT.md D-04, Pin 2). crates/engine's `hwdecode` Cargo feature links the
  LGPL FFmpeg shared libraries DYNAMICALLY via rsmpeg/rusty_ffmpeg; that needs a
  build with include/ headers and import .lib files, which the shipped sidecar
  fetch (fetch-lgpl-ffmpeg.ps1, runtime/binaries/) does not provide -- it ships
  .exe + runtime .dll only, and its rolling `latest` URL is a git-master snapshot
  (avcodec-63) a full soname major ahead of rsmpeg 0.18's avcodec-62 target.

  So this script pins a DATED BtbN autobuild tag -- NEVER the rolling `latest`
  URL, which is the exact failure Phase 44 hit -- asserts the zip's SHA256, and
  then verifies LGPL-cleanliness on the FETCHED BINARY ITSELF (CLAUDE.md rule 6),
  mirroring fetch-lgpl-ffmpeg.ps1's hard-assert. The payload is NOT committed to
  git (see .gitignore); this script reproduces it. See PROVENANCE.md Entry 21.

  The matching build-time guard lives in crates/engine/build.rs: it fails the
  build loudly if these headers are missing or the wrong version.

  NOTE: pure ASCII only -- PowerShell 5.1 reads a UTF-8 (no BOM) .ps1 as ANSI, so
  non-ASCII punctuation (em-dashes, curly quotes) corrupts parsing. Keep it ASCII.

.PARAMETER Force
  Re-download and re-install even if an already-verified payload is present.

.EXAMPLE
  ./scripts/windows/fetch-ffmpeg-devlibs.ps1
#>
param(
  [switch]$Force
)

$ErrorActionPreference = 'Stop'

# ---- The pin (Phase 44 evidence, carried verbatim -- 44-SPIKE-REPORT.md / spikes/44-hwaccel/README.md) ----
$Tag            = 'autobuild-2026-01-31-12-57'
$File           = 'ffmpeg-n8.0.1-48-g0592be14ff-win64-lgpl-shared-8.0.zip'
$Url            = "https://github.com/BtbN/FFmpeg-Builds/releases/download/$Tag/$File"
$ExpectedSha256 = 'C342DB971175D1CDB8101E31B265019A52C75ABB361E91DCB1CE757CC8A2827E'
$ExpectedVer    = 'n8.0.1-48-g0592be14ff'   # what ffmpeg -version must report
$ExpectedAvc    = '62'                       # libavcodec soname major (rsmpeg 0.18 target)

$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$dest = Join-Path $repo 'crates\engine\ffmpeg-dev'

function Invoke-Native {
  # Run a native exe capturing stdout+stderr as one string WITHOUT tripping
  # PowerShell 5.1's "stderr output under ErrorActionPreference=Stop becomes a
  # terminating NativeCommandError" behavior (ffmpeg prints its banner to
  # stderr, so a bare `& $exe ... 2>&1` here would throw).
  param([string]$Exe, [string[]]$NativeArgs)
  $prev = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try { return ((& $Exe @NativeArgs 2>&1) | Out-String) }
  finally { $ErrorActionPreference = $prev }
}

function Test-DevlibsInstall {
  # Returns a list of failure strings; empty list = the installed payload is good.
  # Every check here is run against the ACTUAL installed files/binary, never
  # inferred from the download page (CLAUDE.md rule 6).
  param([string]$Dir)
  $fails = @()
  $ffmpeg = Join-Path $Dir 'bin\ffmpeg.exe'
  if (-not (Test-Path $ffmpeg)) { return @("bin\ffmpeg.exe missing") }

  $ver = Invoke-Native $ffmpeg @('-hide_banner','-version')
  if ($ver -notmatch [regex]::Escape($ExpectedVer)) {
    $fails += "ffmpeg -version does not report $ExpectedVer (got: $(($ver -split "`n" | Select-Object -First 1).Trim()))"
  }
  if ($ver -notmatch "libavcodec\s+$ExpectedAvc\.") {
    $fails += "ffmpeg -version does not report libavcodec $ExpectedAvc"
  }

  # LGPL self-check on the binary: -L must self-report the LESSER GPL.
  $lic = Invoke-Native $ffmpeg @('-hide_banner','-L')
  if ($lic -notmatch 'Lesser General Public License') {
    $fails += "ffmpeg -L does not self-report the GNU Lesser General Public License"
  }
  # Match the ENABLE forms only -- an LGPL build lists --disable-libx264 etc.,
  # so a bare 'libx264' substring check would false-positive on the disable flag.
  $bad = @('--enable-gpl','--enable-nonfree','--enable-libx264','--enable-libx265') |
         Where-Object { $ver -match [regex]::Escape($_) }
  if ($bad) {
    $fails += "build is NOT LGPL-clean (configure enables: $($bad -join ', '))"
  }

  # The dev-build SHAPE the hwdecode feature needs: headers + import libs.
  if (-not (Test-Path (Join-Path $Dir 'include\libavutil\hwcontext_d3d11va.h'))) {
    $fails += "include\libavutil\hwcontext_d3d11va.h missing (not a dev build?)"
  }
  if (-not (Test-Path (Join-Path $Dir 'lib\avcodec.lib'))) {
    $fails += "lib\avcodec.lib missing (no MSVC import libraries?)"
  }
  return $fails
}

# ---- Idempotent fast path: already installed and fully verified -> done ----
if (-not $Force -and (Test-Path $dest)) {
  $fails = Test-DevlibsInstall -Dir $dest
  if ($fails.Count -eq 0) {
    Write-Host "==> ffmpeg-dev already present and verified at $dest (use -Force to re-fetch)" -ForegroundColor Green
    & (Join-Path $dest 'bin\ffmpeg.exe') -hide_banner -version 2>&1 | Select-Object -First 1
    exit 0
  }
  Write-Host "==> Existing ffmpeg-dev FAILED verification -- re-fetching:" -ForegroundColor Yellow
  $fails | ForEach-Object { Write-Host "    $_" -ForegroundColor Yellow }
}

$tmp = Join-Path $env:TEMP ("rudis-ffmpeg-devlibs-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
  Write-Host "==> Downloading pinned LGPL FFmpeg dev build" -ForegroundColor Cyan
  Write-Host "    $Url"
  $zip = Join-Path $tmp $File
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
  Invoke-WebRequest -Uri $Url -OutFile $zip -UseBasicParsing

  Write-Host "==> Asserting SHA256 of the downloaded zip..." -ForegroundColor Cyan
  $actual = (Get-FileHash -Path $zip -Algorithm SHA256).Hash
  if ($actual -ne $ExpectedSha256) {
    throw "SHA256 MISMATCH for ${File}: expected $ExpectedSha256, got $actual. REFUSING to install (supply-chain guard, T-48-03-01)."
  }
  Write-Host "    OK - $actual" -ForegroundColor Green

  Write-Host "==> Extracting..." -ForegroundColor Cyan
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  # The BtbN zip contains a single top-level dir with bin/ include/ lib/ inside.
  $bin = Get-ChildItem -Path $tmp -Recurse -Directory -Filter bin | Select-Object -First 1
  if (-not $bin) { throw "no bin/ directory found inside the archive" }
  $root = $bin.Parent.FullName
  foreach ($sub in @('bin','include','lib')) {
    if (-not (Test-Path (Join-Path $root $sub))) { throw "archive layout unexpected: $sub/ missing under $root" }
  }

  Write-Host "==> Installing into $dest" -ForegroundColor Cyan
  if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
  New-Item -ItemType Directory -Force -Path $dest | Out-Null
  foreach ($sub in @('bin','include','lib')) {
    Copy-Item -Recurse -Force (Join-Path $root $sub) (Join-Path $dest $sub)
  }
  $licTxt = Join-Path $root 'LICENSE.txt'
  if (Test-Path $licTxt) { Copy-Item -Force $licTxt $dest }

  Write-Host "==> Verifying the INSTALLED payload (binary self-check, CLAUDE.md rule 6)..." -ForegroundColor Cyan
  $fails = Test-DevlibsInstall -Dir $dest
  if ($fails.Count -gt 0) {
    $fails | ForEach-Object { Write-Host "    FAIL: $_" -ForegroundColor Red }
    throw "REFUSING to keep this install: verification failed ($($fails.Count) checks)."
  }

  Write-Host "==> Done - ffmpeg-dev installed and verified:" -ForegroundColor Green
  # NEVER 2>&1 a native command under $ErrorActionPreference = 'Stop': PS 5.1
  # wraps native stderr as a terminating error even on exit 0 (repo convention).
  # ffmpeg -version writes its banner to stderr, so route it to $null instead of
  # merging, and treat a missing/odd exit code as non-fatal -- this line is a
  # diagnostic print, not part of the install verification above.
  $verLine = & (Join-Path $dest 'bin\ffmpeg.exe') -hide_banner -version 2>$null | Select-Object -First 1
  if ($verLine) { Write-Host "    $verLine" -ForegroundColor Green }
  $global:LASTEXITCODE = 0
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
