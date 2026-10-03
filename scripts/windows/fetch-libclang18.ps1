<#
.SYNOPSIS
  Fetch the PINNED libclang 18.1.1 (bindgen's backend) into
  crates/engine/libclang/ for the in-process hwdecode build.

.DESCRIPTION
  Phase 48 promotes Phase 44's spike-local libclang pin to production
  infrastructure (48-CONTEXT.md D-04, Pin 1). The version is LOAD-BEARING and
  the failure mode is SILENT: bindgen 0.71.1 (pinned transitively by
  rsmpeg 0.18 -> rusty_ffmpeg ^0.16.7) against libclang 22.1.8 emits
  INCOMPLETE types -- AVFormatContext came out as `{ _address: u8 }` WHILE
  STILL emitting a correct-size layout assertion (472 bytes) -- and the same
  tree builds clean against libclang 18.1.1. There is no in-version escape
  (rusty_ffmpeg 0.17 moved to bindgen ^0.72, but rsmpeg 0.18.0 requires
  rusty_ffmpeg ^0.16.7).

  The DLL comes from the PyPI `libclang` wheel: a small, hash-pinned
  redistribution of an official LLVM libclang.dll (Apache-2.0 WITH
  LLVM-exception). BUILD-TIME TOOL ONLY -- like MSVC or rustc, it is never
  linked into and never ships with any artifact. The payload is NOT committed
  to git (see .gitignore); this script reproduces it. See PROVENANCE.md
  Entry 21.

  This script also writes crates/engine/libclang/libclang.dll.sha256 -- the
  SHA256 of the EXTRACTED DLL, computed at fetch time. crates/engine/build.rs
  compares the on-disk DLL against this sidecar and fails the build LOUDLY on
  mismatch, so the silent-incomplete-types hazard above cannot recur silently.

  NOTE: pure ASCII only -- PowerShell 5.1 reads a UTF-8 (no BOM) .ps1 as ANSI,
  so non-ASCII punctuation corrupts parsing. Keep it ASCII.

.PARAMETER Force
  Re-download and re-install even if an already-verified payload is present.

.EXAMPLE
  ./scripts/windows/fetch-libclang18.ps1
#>
param(
  [switch]$Force
)

$ErrorActionPreference = 'Stop'

# ---- The pin (Phase 44 evidence, carried verbatim -- spikes/44-hwaccel/README.md) ----
$Url = 'https://files.pythonhosted.org/packages/0b/2d/3f480b1e1d31eb3d6de5e3ef641954e5c67430d5ac93b7fa7e07589576c7/libclang-18.1.1-py2.py3-none-win_amd64.whl'
$WhlSha256 = '4DD2D3B82FAB35E2BF9CA717D7B63AC990A3519C7E312F19FA8E86DCC712F7FB'
$InnerPath = 'libclang-18.1.1.data\platlib\clang\native\libclang.dll'

$repo    = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$dest    = Join-Path $repo 'crates\engine\libclang'
$dll     = Join-Path $dest 'libclang.dll'
$sidecar = Join-Path $dest 'libclang.dll.sha256'

# ---- Idempotent fast path: DLL present and matching its recorded hash -> done ----
if (-not $Force -and (Test-Path $dll) -and (Test-Path $sidecar)) {
  $recorded = (Get-Content -Path $sidecar -Raw).Trim()
  $actual   = (Get-FileHash -Path $dll -Algorithm SHA256).Hash
  if ($recorded -and ($actual -ieq $recorded)) {
    Write-Host "==> libclang already present and hash-verified at $dll (use -Force to re-fetch)" -ForegroundColor Green
    exit 0
  }
  Write-Host "==> Existing libclang.dll does NOT match its .sha256 sidecar -- re-fetching" -ForegroundColor Yellow
}

$tmp = Join-Path $env:TEMP ("rudis-libclang18-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
  Write-Host "==> Downloading pinned libclang 18.1.1 wheel" -ForegroundColor Cyan
  Write-Host "    $Url"
  # A .whl is a zip; Expand-Archive requires the .zip extension, so save it as one.
  $zip = Join-Path $tmp 'libclang-18.1.1.zip'
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
  Invoke-WebRequest -Uri $Url -OutFile $zip -UseBasicParsing

  Write-Host "==> Asserting SHA256 of the downloaded wheel..." -ForegroundColor Cyan
  $actual = (Get-FileHash -Path $zip -Algorithm SHA256).Hash
  if ($actual -ine $WhlSha256) {
    throw "SHA256 MISMATCH for the libclang wheel: expected $WhlSha256, got $actual. REFUSING to install (supply-chain guard, T-48-03-01)."
  }
  Write-Host "    OK - $actual" -ForegroundColor Green

  Write-Host "==> Extracting..." -ForegroundColor Cyan
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  $src = Join-Path $tmp $InnerPath
  if (-not (Test-Path $src)) { throw "wheel layout unexpected: $InnerPath not found" }

  Write-Host "==> Installing into $dest" -ForegroundColor Cyan
  New-Item -ItemType Directory -Force -Path $dest | Out-Null
  Copy-Item -Force $src $dll

  # Record the extracted DLL's own hash for crates/engine/build.rs's loud guard.
  $dllHash = (Get-FileHash -Path $dll -Algorithm SHA256).Hash.ToLowerInvariant()
  [IO.File]::WriteAllText($sidecar, $dllHash + "`n", [Text.Encoding]::ASCII)

  Write-Host "==> Done - libclang.dll installed, sidecar written:" -ForegroundColor Green
  Write-Host "    $dll"
  Write-Host "    $sidecar = $dllHash"
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
