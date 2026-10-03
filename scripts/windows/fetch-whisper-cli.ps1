<#
.SYNOPSIS
  Fetch the offline Whisper sidecar (whisper-cli.exe + its runtime DLLs +
  the ggml-small speech model) into runtime/binaries/ for bundling into
  the shipped Windows app.

.DESCRIPTION
  Phase 22 (transcription -> word-cuts -> captions -> transcript-search) needs an
  OFFLINE speech-to-text sidecar. whisper.cpp's prebuilt Windows CLI + a GGML
  Whisper model give exactly that, and are MIT-licensed (redistribution-safe in a
  closed-source PAID app -- see PROVENANCE.md Entry 11). This mirrors EXACTLY how
  Phase 10 bundled the LGPL FFmpeg sidecar (scripts/windows/fetch-lgpl-ffmpeg.ps1,
  PROVENANCE Entry 5): a fetch-and-hard-verify script, gitignored binaries under
  runtime/binaries (staged beside the exe on `dotnet publish`), a PROVENANCE entry.
  engine::whisper::locate_whisper() (Plan 02) resolves
  the bundled binary first, exactly like engine::locate() does for ffmpeg.

  The integrity control here is a SHA256 hash-verify of BOTH artifacts against pinned
  constants (whisper.cpp is MIT, so unlike the FFmpeg script there is no GPL config to
  reject -- the supply-chain hash gate IS the security control, threat T-22-01). The
  MIT license is ALSO surfaced explicitly (threat T-22-02): the script live-fetches the
  whisper.cpp LICENSE at the pinned tag and hard-asserts it is an MIT License, and
  records the model's MIT license source, rather than silently trusting.

  The binaries are NOT committed to git (see .gitignore: /runtime/binaries/ already
  covers *.exe, *.bin and *.dll); this script reproduces them.

  NOTE: pure ASCII only -- PowerShell 5.1 reads a UTF-8 (no BOM) .ps1 as ANSI, so
  non-ASCII punctuation (em-dashes, curly quotes) corrupts parsing. Keep it ASCII.

.PARAMETER Url
  The pinned whisper.cpp Windows x64 release zip (contains whisper-cli.exe + the
  ggml*/whisper runtime DLLs). Defaults to the v1.9.1 tagged release asset. Override
  to pin a different reproducible release.

.PARAMETER ModelUrl
  The pinned GGML Whisper model. Defaults to ggml-small.bin (multilingual small,
  ~488 MB) from huggingface.co/ggerganov/whisper.cpp. The --dtw preset that matches
  this model is 'small' (see the RESOLVED WHISPER-CLI FLAGS block below).

.EXAMPLE
  ./scripts/windows/fetch-whisper-cli.ps1
#>

# =============================================================================
# RESOLVED WHISPER-CLI FLAGS (verified against whisper.cpp v1.9.1 --help on 2026-07-12
# by running the REAL pinned binary -- runtime/binaries/whisper-cli.exe --help;
# NOT guessed. This block is the single source of truth Plan 02's
# crates/engine/src/whisper.rs copies its argv from -- resolves Open Question 3,
# Pitfall 1 (flag drift) and Pitfall 2 (-ml 1 is NOT --dtw).):
#
#   WORD/TOKEN TIMESTAMPS (DTW cross-attention alignment):
#     -dtw MODEL        (aka --dtw MODEL)  "compute token-level timestamps"
#       NOTE: SINGLE dash form is what the binary prints (-dtw, not --dtw).
#       The MODEL argument is an alignment-head PRESET name that must match the
#       bundled model. For the bundled ggml-small.bin the preset is:  -dtw small
#       VERIFIED: '-dtw small' passes preset-parse (proceeds to model load), while
#       '-dtw boguspreset' is rejected with "error: unknown DTW preset 'boguspreset'".
#       Do NOT substitute '-ml 1' (Pitfall 2 -- that is a cruder per-word split
#       heuristic, not DTW cross-attention alignment).
#
#   JSON OUTPUT (per-token detail with start/end offsets):
#     -oj               (aka --output-json)       "output result in a JSON file"
#     -ojf              (aka --output-json-full)  "include more information in the JSON file"
#       Use -ojf for per-token timing detail (Pitfall 1 warned -ojf might mean
#       "json to file"; the REAL v1.9.1 --help says -ojf == --output-json-full ==
#       more detail -- recorded verbatim here to kill the ambiguity). whisper-cli
#       writes <of>.json when -oj/-ojf is set.
#     -of FNAME         (aka --output-file FNAME) "output file path (without file extension)"
#
#   INPUT CONTRACT (16 kHz mono WAV):
#     -f FNAME          (aka --file FNAME)  "input audio file path"  (positional
#       "file0 file1 ..." also accepted). whisper.cpp requires 16 kHz mono WAV;
#       the caller must resample (reuse render_audio_pcm's ffmpeg sidecar).
#     -m FNAME          (aka --model FNAME) "model path" -> runtime/binaries/ggml-small.bin
#     -l LANG           (aka --language LANG) default 'en'; 'auto' to auto-detect.
#
# LINK SHAPE: dynamic (DLLs beside whisper-cli.exe: ggml.dll, ggml-base.dll,
#   ggml-cpu-{alderlake,cannonlake,cascadelake,haswell,icelake,sandybridge,skylakex,
#   sse42,x64}.dll [10 CPU-arch variants], whisper.dll [+ parakeet.dll, SDL2.dll for
#   sibling tools]). whisper-cli.exe dies at launch with a DLL-not-found error if these
#   are absent, so the *.dll copy step below is LOAD-BEARING (BLOCKER-1), exactly like
#   the LGPL FFmpeg shared build. VERIFIED: installed whisper-cli.exe reports
#   "whisper.cpp version: 1.9.1" only with the DLLs beside it.
# =============================================================================

param(
  [string]$Url      = "https://github.com/ggml-org/whisper.cpp/releases/download/v1.9.1/whisper-bin-x64.zip",
  [string]$ModelUrl = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin"
)

$ErrorActionPreference = 'Stop'

# --- Pinned integrity + license constants (threat T-22-01 supply-chain gate) -------
# SHA256 of whisper-cli.exe inside the v1.9.1 whisper-bin-x64.zip (verified 2026-07-12).
$ExpectedBinSha   = '58245314fb73b30fbd0cf0542c5c172e23f02b6eb7cad7b51e792439cf5e1755'
# SHA256 of ggml-small.bin (== HuggingFace git-lfs oid, size 487601967, verified 2026-07-12).
$ExpectedModelSha = '1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b'
# Pinned tag whose MIT LICENSE is live-asserted below (threat T-22-02).
$WhisperTag       = 'v1.9.1'
$LicenseUrl       = "https://raw.githubusercontent.com/ggml-org/whisper.cpp/$WhisperTag/LICENSE"
# License determination surfaced explicitly (do NOT silently trust). All THREE MIT:
#   whisper.cpp code    -> MIT  (github.com/ggml-org/whisper.cpp/blob/master/LICENSE)
#   OpenAI Whisper weights -> MIT (github.com/openai/whisper/blob/main/LICENSE)
#   GGML model conversion  -> MIT (huggingface.co/ggerganov/whisper.cpp model card)
$ExpectedLicense  = 'MIT'

$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$dest = Join-Path $repo 'runtime\binaries'
$tmp  = Join-Path $env:TEMP ("rudis-whisper-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

  # --- 1. Live MIT-license assert (do not silently trust) --------------------------
  Write-Host "==> Verifying whisper.cpp license is $ExpectedLicense at $WhisperTag" -ForegroundColor Cyan
  $licenseText = (Invoke-WebRequest -Uri $LicenseUrl -UseBasicParsing).Content
  if ($licenseText -notmatch "$ExpectedLicense License") {
    throw "REFUSING to bundle: whisper.cpp LICENSE at $WhisperTag does not assert an $ExpectedLicense License."
  }
  Write-Host "    OK - $ExpectedLicense License confirmed (code MIT / OpenAI weights MIT / GGML conversion MIT)" -ForegroundColor Green

  # --- 2. Download + extract the Windows CLI release -------------------------------
  Write-Host "==> Downloading whisper.cpp Windows CLI" -ForegroundColor Cyan
  Write-Host "    $Url"
  $zip = Join-Path $tmp 'whisper-win64.zip'
  Invoke-WebRequest -Uri $Url -OutFile $zip -UseBasicParsing

  Write-Host "==> Extracting..." -ForegroundColor Cyan
  Expand-Archive -Path $zip -DestinationPath $tmp -Force

  # Locate the CLI executable, checking BOTH the current name (whisper-cli.exe) and
  # the legacy name (main.exe) -- Assumption A2. Prefer whisper-cli.exe.
  $cli = Get-ChildItem -Path $tmp -Recurse -File -Filter 'whisper-cli.exe' | Select-Object -First 1
  if (-not $cli) {
    $cli = Get-ChildItem -Path $tmp -Recurse -File -Filter 'main.exe' | Select-Object -First 1
  }
  if (-not $cli) { throw "no whisper-cli.exe or main.exe found inside the archive" }
  $binDir = $cli.Directory.FullName
  Write-Host "    found CLI: $($cli.Name) in $binDir" -ForegroundColor Green

  # --- 3. SHA256-verify the binary (threat T-22-01) --------------------------------
  Write-Host "==> Verifying binary integrity..." -ForegroundColor Cyan
  $binSha = (Get-FileHash -Algorithm SHA256 -Path $cli.FullName).Hash.ToLower()
  if ($binSha -ne $ExpectedBinSha.ToLower()) {
    throw "REFUSING to bundle: whisper-cli.exe SHA256 mismatch (got $binSha expected $($ExpectedBinSha.ToLower()))."
  }
  Write-Host "    OK - whisper-cli.exe SHA256 verified ($($ExpectedBinSha.ToLower()))" -ForegroundColor Green

  # --- 4. Download + SHA256-verify the GGML model ----------------------------------
  Write-Host "==> Downloading GGML model (ggml-small.bin, ~488 MB)" -ForegroundColor Cyan
  Write-Host "    $ModelUrl"
  $model = Join-Path $tmp 'ggml-small.bin'
  Invoke-WebRequest -Uri $ModelUrl -OutFile $model -UseBasicParsing
  Write-Host "==> Verifying model integrity..." -ForegroundColor Cyan
  $modelSha = (Get-FileHash -Algorithm SHA256 -Path $model).Hash.ToLower()
  if ($modelSha -ne $ExpectedModelSha.ToLower()) {
    throw "REFUSING to bundle: ggml-small.bin SHA256 mismatch (got $modelSha expected $($ExpectedModelSha.ToLower()))."
  }
  Write-Host "    OK - ggml-small.bin SHA256 verified ($($ExpectedModelSha.ToLower()))" -ForegroundColor Green

  # --- 5. Install into runtime\binaries\ --------------------------------------------
  Write-Host "==> Installing into $dest" -ForegroundColor Cyan
  New-Item -ItemType Directory -Force -Path $dest | Out-Null
  # Canonical name is whisper-cli.exe even if the release shipped main.exe.
  Copy-Item $cli.FullName (Join-Path $dest 'whisper-cli.exe') -Force
  Copy-Item $model (Join-Path $dest 'ggml-small.bin') -Force
  # BLOCKER-1: the release is DYNAMICALLY linked -- bring every runtime DLL beside
  # the CLI (ggml*.dll / whisper.dll / ...) or whisper-cli.exe dies at launch with
  # DLL-not-found. If a future pinned release were statically linked, this copies
  # zero DLLs (a documented harmless no-op).
  Get-ChildItem -Path $binDir -Filter '*.dll' | ForEach-Object { Copy-Item $_.FullName $dest -Force }

  $count = (Get-ChildItem $dest -File | Measure-Object).Count
  $dlls  = (Get-ChildItem $dest -File -Filter '*.dll' | Measure-Object).Count
  Write-Host "==> Done - $count files in runtime\binaries\ (whisper-cli.exe + ggml-small.bin + $dlls DLLs)" -ForegroundColor Green
  Write-Host "    Bundled whisper-cli reports:" -ForegroundColor Green
  & (Join-Path $dest 'whisper-cli.exe') --version 2>&1 | Select-String 'whisper.cpp version' | Select-Object -First 1
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
