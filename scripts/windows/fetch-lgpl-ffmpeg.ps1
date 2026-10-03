<#
.SYNOPSIS
  Fetch a license-safe LGPL FFmpeg (ffmpeg.exe + ffprobe.exe + runtime DLLs)
  into runtime/binaries/ for bundling into the shipped Windows app.

.DESCRIPTION
  TWO INDEPENDENT LICENSE AXES, BOTH HARD-ENFORCED HERE. This script THROWS; it
  never merely warns, and every assert runs BEFORE runtime\binaries is touched.

  1. COPYLEFT. CLAUDE.md / STACK.md rule: the shipped product must use an LGPL
     FFmpeg build (no --enable-gpl, no --enable-nonfree, no libx264, no libx265),
     dynamically linked, invoked as a sidecar subprocess -- never a GPL build and
     never copied GPL source. A dev machine's ffmpeg on PATH is often a GPL build
     (e.g. gyan --enable-gpl --enable-libx264): fine for local dev, must not ship.
     engine::locate() prefers bundled binaries over PATH: first RUDIS_FFMPEG_DIR (a
     manual/test override -- nothing in the shipped app sets it), then the directory
     beside the running exe, where `dotnet publish` stages this payload (crates/ffi/
     Rudis.Ffi.targets StageRustFfiPublish), then PATH as a dev-only fallback.

  2. PATENT SCOPE (Phase 62 / SHIP-04, 2026-08-30). Three patent-encumbered
     SOFTWARE encoders must not be present in the shipped binary at all:
       libopenh264   H.264 / AVC   Cisco's binary patent grant covers only Cisco's
                                   OWN separately-downloaded binary, explicitly not
                                   one "integrated into or combined with third party
                                   software"; a self-built one shifts liability to
                                   the builder.
       libkvazaar    H.265 / HEVC  No meaningful free tier, 3+ fragmented pools plus
                                   17+ unpooled holders, per-unit cost from unit one.
       libvvenc      H.266 / VVC   A third, essentially unpriced pool.
     Rudis selects NONE of them (DEFAULT_VIDEO_ENCODER = "h264_mf", no HEVC export
     path), so removing them costs zero features. Until 2026-08-30 this script only
     WARNED about libopenh264 and never looked at the other two; it now hard-fails
     on all three, and that hard-fail has been WATCHED FIRING against a contaminated
     build (Phase 62 plan 62-01).

  Conversely, h264_mf (H.264 via Windows Media Foundation) MUST be present: it is
  the product's default video encoder, so a payload without it would break every
  export while looking like a licensing success. Its absence is also a hard fail.

  SOURCE. The default is a PINNED release asset of aaasocial/FFmpeg-Builds, a
  PUBLIC fork of BtbN/FFmpeg-Builds whose only deltas are build-script changes: it
  drops those three encoders, and it PINS THE FFMPEG SOURCE REVISION to
  a7e72069f1 (N-125907) instead of taking rolling master HEAD. That second pin is
  load-bearing: Phase 60 calibrated export's NVENC operating point (qp 20) and its
  146x50..4096x4096 canvas window against exactly that build and asserts the build
  id in crates/engine/tests/export_hw_encoder.rs, so a rolling HEAD would silently
  invalidate a signed-off calibration. The ONLY difference between the calibrated
  binary and the one this script installs is the three removed encoders. No FFmpeg
  source is modified. The download is verified against a pinned SHA256. The binaries are NOT committed to git (see
  .gitignore); this script reproduces them. See PROVENANCE.md Entry 5.

  ORDERING IS LOAD-BEARING. Every assert runs against the extracted copy in $tmp,
  BEFORE anything in runtime\binaries is touched, so a failing assert leaves the
  installed payload byte-identical. Do not reorder.

  STAGING IS SURGICAL. runtime\binaries also holds sidecars owned by OTHER fetch
  scripts (whisper-cli.exe, whisper.dll, ggml*.dll, ggml-small.bin, parakeet.dll,
  SDL2.dll, opencv\). Only ffmpeg-owned files are replaced, and the survival of
  everything else is asserted afterwards. The pre-2026-08-30 version of this script did
  Remove-Item -Recurse -Force $dest and would have destroyed all of them.

  NOTE: pure ASCII only -- PowerShell 5.1 reads a UTF-8 (no BOM) .ps1 as ANSI, so
  non-ASCII punctuation (em-dashes, curly quotes) corrupts parsing. Keep it ASCII.

.PARAMETER Url
  The LGPL-shared archive URL. Defaults to the pinned Rudis fork release asset.
  Overriding this without also passing -Sha256 disables the integrity pin and
  prints a loud provenance warning; the license asserts still run.

.PARAMETER Sha256
  Expected SHA256 of the downloaded archive. Defaults to the pinned hash that
  matches the default -Url. Pass an empty string to skip (not recommended).

.EXAMPLE
  ./scripts/windows/fetch-lgpl-ffmpeg.ps1

.EXAMPLE
  # Deliberately contaminated build -- this MUST fail (that is the point):
  ./scripts/windows/fetch-lgpl-ffmpeg.ps1 -Url "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-lgpl-shared.zip"
#>
param(
  [string]$Url    = "https://github.com/aaasocial/FFmpeg-Builds/releases/download/rudis-lgpl-n125907-2026-08-30/ffmpeg-N-125907-ga7e72069f1-win64-lgpl-shared.zip",
  [string]$Sha256 = "a7ddb2a15fa2930beb39a2e8596a79a7557079928c0ed615dfd3f2896ef1ab70"
)

$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$dest = Join-Path $repo 'runtime\binaries'
$tmp  = Join-Path $env:TEMP ("rudis-ffmpeg-lgpl-" + [guid]::NewGuid().ToString('N'))

# --- Patent-scope blocklist (SHIP-04). Checked on TWO independent axes: the
# --- configure flags reported by -version/-buildconf, and the actual encoder rows
# --- reported by -encoders (which catches a build whose buildconf lies or omits).
$BannedEncoders = @('libopenh264','libkvazaar','libvvenc')
# --- Copyleft blocklist. Match the ENABLE forms only: an LGPL build legitimately
# --- lists --disable-libx264 etc., so a bare substring check would false-positive.
$BannedConfigure = @(
  '--enable-gpl','--enable-nonfree','--enable-libx264','--enable-libx265',
  '--enable-libopenh264','--enable-libkvazaar','--enable-libvvenc'
)
# --- The product's default video encoder. Its ABSENCE is a hard fail.
$RequiredEncoder = 'h264_mf'

# runtime\binaries is shared: these name patterns are the ONLY things this script
# owns and is allowed to delete. Everything else there belongs to another fetcher.
function Test-FfmpegOwned {
  param([string]$Name)
  if ($Name -match '^(ffmpeg|ffprobe|ffplay)\.exe$') { return $true }
  if ($Name -match '^(av|sw|postproc)[A-Za-z0-9_.-]*\.dll$') { return $true }
  # NOT SDL2.dll: despite sitting next to the ffmpeg binaries it is WHISPER's. It
  # ships inside whisper.cpp's whisper-bin-x64.zip and is installed by
  # fetch-whisper-cli.ps1 ("whisper.dll [+ parakeet.dll, SDL2.dll for sibling
  # tools]"). The FFmpeg archive carries no SDL2.dll at all, so claiming ownership
  # of it here would delete a whisper sidecar on every run and never put it back.
  return $false
}

function Find-EncoderRow {
  param([string[]]$Lines, [string]$Name)
  $pat = '^\s*\S+\s+' + [regex]::Escape($Name) + '(\s|$)'
  return @($Lines | Where-Object { $_ -match $pat }).Count -gt 0
}

# Runs the patent-scope axis against an ffmpeg.exe. Throws on any violation.
function Assert-EncoderSetClean {
  param([string]$FfmpegPath, [string]$Label)
  $enc   = & $FfmpegPath -hide_banner -encoders 2>&1 | Out-String
  $lines = $enc -split "\r?\n"
  $hits  = @($BannedEncoders | Where-Object { Find-EncoderRow -Lines $lines -Name $_ })
  if ($hits.Count -gt 0) {
    throw ("REFUSING to bundle ({0}): patent-encumbered encoders present in -encoders: {1}. " -f $Label, ($hits -join ', ')) +
          "SHIP-04 requires a build configured --disable-libopenh264 --disable-libkvazaar --disable-libvvenc."
  }
  if (-not (Find-EncoderRow -Lines $lines -Name $RequiredEncoder)) {
    throw ("REFUSING to bundle ({0}): required encoder '{1}' is MISSING from -encoders. " -f $Label, $RequiredEncoder) +
          "That is the product's DEFAULT_VIDEO_ENCODER; a payload without it breaks every export."
  }
}

$urlOverridden = $PSBoundParameters.ContainsKey('Url')
$shaSupplied   = $PSBoundParameters.ContainsKey('Sha256')
if ($urlOverridden -and -not $shaSupplied) {
  Write-Host ""
  Write-Host "!! PROVENANCE WARNING -------------------------------------------------" -ForegroundColor Yellow
  Write-Host "!! -Url was overridden but no -Sha256 was given, so the integrity pin is" -ForegroundColor Yellow
  Write-Host "!! DISABLED for this run. The archive will be accepted on the strength of" -ForegroundColor Yellow
  Write-Host "!! the license asserts alone. Do NOT ship a payload fetched this way -- a" -ForegroundColor Yellow
  Write-Host "!! shipped build must be reproducible from a pinned URL + SHA256 recorded" -ForegroundColor Yellow
  Write-Host "!! in PROVENANCE.md." -ForegroundColor Yellow
  Write-Host "!! ---------------------------------------------------------------------" -ForegroundColor Yellow
  Write-Host ""
  $Sha256 = ''
}

New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
  Write-Host "==> Downloading LGPL FFmpeg" -ForegroundColor Cyan
  Write-Host "    $Url"
  $zip = Join-Path $tmp 'ffmpeg-lgpl.zip'
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
  Invoke-WebRequest -Uri $Url -OutFile $zip -UseBasicParsing

  Write-Host "==> Verifying archive integrity..." -ForegroundColor Cyan
  $actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToUpperInvariant()
  if ($Sha256) {
    $expected = $Sha256.Trim().ToUpperInvariant()
    if ($actual -ne $expected) {
      throw "REFUSING to bundle: SHA256 mismatch. expected=$expected actual=$actual"
    }
    Write-Host "    OK - SHA256 matches the pin: $actual" -ForegroundColor Green
  } else {
    Write-Host "    SKIPPED (no pin for this URL). Archive SHA256 = $actual" -ForegroundColor Yellow
  }

  Write-Host "==> Extracting..." -ForegroundColor Cyan
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  $bin = Get-ChildItem -Path $tmp -Recurse -Directory -Filter bin | Select-Object -First 1
  if (-not $bin) { throw "no bin/ directory found inside the archive" }
  $ffmpeg  = Join-Path $bin.FullName 'ffmpeg.exe'
  $ffprobe = Join-Path $bin.FullName 'ffprobe.exe'
  if (-not (Test-Path $ffmpeg))  { throw "ffmpeg.exe not found in archive bin/" }
  if (-not (Test-Path $ffprobe)) { throw "ffprobe.exe not found in archive bin/" }

  # ---------------------------------------------------------------------------
  # ALL ASSERTS RUN HERE, against $tmp, BEFORE runtime\binaries is touched.
  # ---------------------------------------------------------------------------
  Write-Host "==> Axis 1/2: configure flags (copyleft + patent scope)..." -ForegroundColor Cyan
  $cfg = & $ffmpeg -hide_banner -version 2>&1 | Out-String
  $bad = @($BannedConfigure | Where-Object { $cfg -match [regex]::Escape($_) })
  if ($bad.Count -gt 0) {
    throw "REFUSING to bundle: build configuration contains forbidden flags (found: $($bad -join ', ')). " +
          "Copyleft rule: no gpl/nonfree/libx264/libx265. SHIP-04 patent rule: no libopenh264/libkvazaar/libvvenc."
  }
  $ver = ($cfg -split "\r?\n" | Select-Object -First 1).Trim()
  Write-Host "    OK - no forbidden --enable flags: $ver" -ForegroundColor Green

  Write-Host "==> Axis 2/2: actual encoder set (-encoders)..." -ForegroundColor Cyan
  Assert-EncoderSetClean -FfmpegPath $ffmpeg -Label 'downloaded archive'
  Write-Host "    OK - none of $($BannedEncoders -join ', '); $RequiredEncoder present" -ForegroundColor Green

  # ---------------------------------------------------------------------------
  # SURGICAL STAGING. runtime\binaries is shared with other fetch scripts, so
  # only ffmpeg-owned files are removed. NEVER Remove-Item -Recurse $dest.
  # ---------------------------------------------------------------------------
  Write-Host "==> Installing into $dest" -ForegroundColor Cyan
  New-Item -ItemType Directory -Force -Path $dest | Out-Null
  $survivors = @(Get-ChildItem -Path $dest -Force |
                 Where-Object { -not (Test-FfmpegOwned $_.Name) } |
                 ForEach-Object { $_.Name })
  if ($survivors.Count -gt 0) {
    Write-Host "    preserving $($survivors.Count) non-ffmpeg entries (whisper/ggml/opencv/...)" -ForegroundColor DarkGray
  }
  Get-ChildItem -Path $dest -File | Where-Object { Test-FfmpegOwned $_.Name } | Remove-Item -Force

  Copy-Item $ffmpeg  $dest -Force
  Copy-Item $ffprobe $dest -Force
  # Shared LGPL build: bring the runtime DLLs (avcodec/avformat/...) alongside.
  # Filtered by the same ownership test as the delete, so an archive that happened
  # to carry a sibling's DLL could not silently overwrite the sibling's copy.
  Get-ChildItem -Path $bin.FullName -Filter *.dll |
    Where-Object { Test-FfmpegOwned $_.Name } |
    ForEach-Object { Copy-Item $_.FullName $dest -Force }

  # Tripwire for the exact regression the old destructive staging would have caused.
  $lost = @($survivors | Where-Object { -not (Test-Path (Join-Path $dest $_)) })
  if ($lost.Count -gt 0) {
    throw "STAGING BUG: non-ffmpeg sidecars were destroyed by this script (missing: $($lost -join ', ')). " +
          "runtime\binaries is shared with fetch-whisper-cli.ps1 / fetch-opencv-sdk.ps1."
  }

  # Post-install proof on the SHIPPED path, not just the temp copy.
  Write-Host "==> Re-verifying the SHIPPED binary..." -ForegroundColor Cyan
  $shipped = Join-Path $dest 'ffmpeg.exe'
  Assert-EncoderSetClean -FfmpegPath $shipped -Label 'shipped runtime\binaries'
  Write-Host "    OK - shipped ffmpeg.exe carries none of $($BannedEncoders -join ', ')" -ForegroundColor Green
  Write-Host "    OK - shipped ffmpeg.exe still provides $RequiredEncoder" -ForegroundColor Green

  $count = (Get-ChildItem $dest -File | Measure-Object).Count
  Write-Host "==> Done - $count files in runtime\binaries\ (ffmpeg.exe + ffprobe.exe + DLLs + sidecars)" -ForegroundColor Green
  Write-Host "    Bundled ffmpeg reports:" -ForegroundColor Green
  & $shipped -hide_banner -version 2>&1 | Select-Object -First 1
}
catch {
  Write-Host ""
  Write-Host "FAILED: $($_.Exception.Message)" -ForegroundColor Red
  Write-Host "runtime\binaries was NOT modified by this run unless the message says otherwise." -ForegroundColor Red
  exit 1
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
