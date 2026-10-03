<#
.SYNOPSIS
  Fetch the license-clean OFFLINE object-tracking sidecar (a PSF-embeddable
  Python + opencv-contrib-python + numpy + track_cli.py) into
  runtime/binaries/opencv/ for the Phase-30 `tracking` feature.

.DESCRIPTION
  WAVE-0 DECISION (Plan 30-01): the tracking backend resolved to the **FALLBACK**
  -- a Python `opencv-contrib-python` sidecar -- NOT the native `opencv` crate.
  Reason: the license-clean, contrib-FREE official OpenCV Windows SDK does NOT
  ship TrackerCSRT/TrackerKCF (they live only in opencv_contrib's `tracking`
  module; the main `video` module carries only MIL + weighted DNN trackers). SC-1
  mandates CSRT, and `opencv-contrib-python` is the only license-safe, weightless,
  reproducible way to get real CSRT/KCF on Windows (a prebuilt MIT-wrapper /
  Apache-2.0-OpenCV wheel -- no from-source opencv_contrib build, no vcpkg risk).
  See PROVENANCE.md Entry 12 + crates/engine/src/tracking.rs header.

  This mirrors EXACTLY how Phase 10 bundled the LGPL FFmpeg sidecar
  (fetch-lgpl-ffmpeg.ps1, PROVENANCE Entry 5) and Phase 22 bundled whisper.cpp
  (fetch-whisper-cli.ps1, PROVENANCE Entry 11): fetch-and-hard-verify, gitignored
  binaries under runtime/binaries/, resolved bundled-first (never a
  PATH-resolved python in a shipped build -- threat T-30-01).

  Integrity + license controls (do NOT silently trust):
    * SHA256-verify the PSF embeddable Python zip against a pinned constant and
      `throw` on mismatch (supply-chain gate, threat T-30-02 -- the analogue of
      Entry 5's LGPL hard-assert / Entry 11's SHA gate).
    * PIN exact package versions (opencv-contrib-python==5.0.0.93, numpy==2.5.1).
    * HARD-ASSERT the wheel exposes the WEIGHTLESS classical trackers
      cv2.TrackerCSRT_create + cv2.TrackerKCF_create (SC-1), and that NO model
      weight file (*.caffemodel/*.onnx/*.pb/*.pt) and NO CoTracker reference is
      bundled (SC-3). `throw` if any is found.

  The bundle is CONFINED to runtime/binaries/opencv/ (never a global/PATH
  location) and NOT committed to git (.gitignore excludes /runtime/binaries/);
  this script reproduces it. The authored sidecar CLI scripts/windows/track_cli.py
  IS committed and is copied into the bundle here.

  NOTE: pure ASCII only -- PowerShell 5.1 reads a UTF-8 (no BOM) .ps1 as ANSI, so
  non-ASCII punctuation corrupts parsing. Keep it ASCII.

.PARAMETER PythonUrl
  The pinned PSF Windows EMBEDDABLE package zip. Defaults to 3.12.0 (ABI-matched
  to the opencv-contrib-python cp312 wheel). Override to re-pin (update $ExpectedPySha).
#>
param(
  [string]$PythonUrl = "https://www.python.org/ftp/python/3.12.0/python-3.12.0-embed-amd64.zip"
)

$ErrorActionPreference = 'Stop'

# --- Pinned integrity + version constants ----------------------------------------
# SHA256 of python-3.12.0-embed-amd64.zip (PSF embeddable, verified 2026-07-20).
$ExpectedPySha = 'c87f000e3dae1a572e98e81daeb622f8bc6f22664093fc9c70989b5f0018d49b'
# Pinned package versions (cp312 wheels; ABI-matched to the 3.12 embeddable).
$OpenCvPkg = 'opencv-contrib-python==5.0.0.93'  # MIT wrapper / Apache-2.0 OpenCV, contrib tracking module
$NumpyPkg  = 'numpy==2.5.1'
$GetPipUrl = 'https://bootstrap.pypa.io/get-pip.py'

$repo    = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$dest    = Join-Path $repo 'runtime\binaries\opencv'
$pydir   = Join-Path $dest 'python'
$sp      = Join-Path $pydir 'site-packages'
$cli_src = Join-Path $PSScriptRoot 'track_cli.py'
$test_src = Join-Path $PSScriptRoot 'track_cli_test.py'
$tmp     = Join-Path $env:TEMP ("rudis-track-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

  # --- 1. Download + SHA256-verify the PSF embeddable Python (threat T-30-02) -----
  Write-Host "==> Downloading PSF embeddable Python" -ForegroundColor Cyan
  Write-Host "    $PythonUrl"
  $zip = Join-Path $tmp 'py-embed.zip'
  Invoke-WebRequest -Uri $PythonUrl -OutFile $zip -UseBasicParsing
  $sha = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLower()
  if ($sha -ne $ExpectedPySha.ToLower()) {
    throw "REFUSING to install: embeddable Python SHA256 mismatch (got $sha expected $($ExpectedPySha.ToLower()))."
  }
  Write-Host "    OK - embeddable Python SHA256 verified ($sha)" -ForegroundColor Green

  # --- 2. Extract the embeddable + enable site-packages ----------------------------
  Write-Host "==> Installing embeddable into $pydir (confined; never PATH-global)" -ForegroundColor Cyan
  if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
  New-Item -ItemType Directory -Force -Path $pydir | Out-Null
  Expand-Archive -Path $zip -DestinationPath $pydir -Force
  New-Item -ItemType Directory -Force -Path $sp | Out-Null
  # Enable `import site` + the bundled site-packages in the ._pth (embeddable
  # python ships with site disabled).
  $pth = Get-ChildItem -Path $pydir -Filter 'python*._pth' | Select-Object -First 1
  if (-not $pth) { throw "no python*._pth in the embeddable" }
  $pthBody = @('python312.zip', '.', 'site-packages', '', 'import site')
  Set-Content -Path $pth.FullName -Value $pthBody -Encoding ASCII

  $python = Join-Path $pydir 'python.exe'

  # --- 3. Bootstrap pip in the embeddable + install the pinned wheels --------------
  Write-Host "==> Bootstrapping pip (get-pip.py) in the embeddable" -ForegroundColor Cyan
  $getpip = Join-Path $tmp 'get-pip.py'
  Invoke-WebRequest -Uri $GetPipUrl -OutFile $getpip -UseBasicParsing
  & $python $getpip --no-warn-script-location | Out-Null
  Write-Host "==> Installing pinned wheels into $sp" -ForegroundColor Cyan
  Write-Host "    $OpenCvPkg  +  $NumpyPkg"
  & $python -m pip install --no-warn-script-location --target $sp $OpenCvPkg $NumpyPkg | Out-Null

  # --- 4. Copy the committed sidecar CLI (+ its pure-stdlib test) into the bundle --
  Copy-Item $cli_src (Join-Path $dest 'track_cli.py') -Force
  if (Test-Path $test_src) {
    Copy-Item $test_src (Join-Path $dest 'track_cli_test.py') -Force
  }

  # --- 5. HARD-ASSERT weightless classical CSRT/KCF present (SC-1) ------------------
  Write-Host "==> Asserting cv2 CSRT/KCF (weightless classical trackers) present..." -ForegroundColor Cyan
  $probe = @'
import sys, cv2
assert hasattr(cv2, "TrackerCSRT_create"), "cv2.TrackerCSRT_create missing"
assert hasattr(cv2, "TrackerKCF_create"),  "cv2.TrackerKCF_create missing"
t = cv2.TrackerCSRT_create()  # must construct with no model weights
sys.stdout.write("cv2 " + cv2.__version__ + " CSRT+KCF OK")
'@
  $probeFile = Join-Path $tmp 'probe.py'
  Set-Content -Path $probeFile -Value $probe -Encoding ASCII
  $probeOut = & $python $probeFile 2>&1 | Out-String
  if ($LASTEXITCODE -ne 0) {
    throw "REFUSING to install: cv2 CSRT/KCF assert failed: $probeOut"
  }
  Write-Host "    OK - $($probeOut.Trim())" -ForegroundColor Green

  # --- 6. HARD-ASSERT no model weights + no CoTracker in the bundle (SC-3) ----------
  Write-Host "==> Asserting NO model weights / NO CoTracker in the bundle..." -ForegroundColor Cyan
  $weights = Get-ChildItem -Path $dest -Recurse -File -Include *.caffemodel,*.onnx,*.pb,*.pt,*.pth,*.weights -ErrorAction SilentlyContinue
  if ($weights) {
    throw "REFUSING to install: model weight file(s) bundled (SC-3 violation): $($weights.FullName -join ', ')"
  }
  $cotracker = Get-ChildItem -Path $dest -Recurse -File -ErrorAction SilentlyContinue |
               Where-Object { $_.Name -match '(?i)cotracker' } | Select-Object -First 1
  if ($cotracker) {
    throw "REFUSING to install: CoTracker (CC-BY-NC) reference bundled: $($cotracker.FullName)"
  }
  Write-Host "    OK - no weight files, no CoTracker (CSRT/KCF are weightless correlation filters)" -ForegroundColor Green

  $count = (Get-ChildItem $dest -Recurse -File | Measure-Object).Count
  Write-Host "==> Done - tracking sidecar installed under runtime\binaries\opencv\ ($count files)" -ForegroundColor Green
  Write-Host "    python:     $python"
  Write-Host "    track_cli:  $(Join-Path $dest 'track_cli.py')"
  Write-Host "    Verify:     cargo test -p engine --features tracking trackercsrt_init_update_smoke"
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
