<#
.SYNOPSIS
  Write SHA256SUMS.txt for the user-facing release assets (Phase 72, SHIP-01 rescoped, D-02).

.DESCRIPTION
  Rudis ships UNSIGNED (Phase 72, D-01). Integrity is published instead: every
  user-facing asset of a GitHub Release is listed in SHA256SUMS.txt, and a user
  (or verify-release-checksums.ps1, or GNU `sha256sum -c`) can recompute and
  compare.

  FORMAT. The standard sha256sum text format, one line per asset:
    <64 lowercase hex><two spaces><bare filename>
  Hashes come from Get-FileHash -LiteralPath -Algorithm SHA256 (streamed), lowercased.

  LF / ASCII / NO BOM. The file is written with
    [System.IO.File]::WriteAllText($path, (($lines -join "`n") + "`n"), (New-Object System.Text.ASCIIEncoding))
  and NEVER with Set-Content / Out-File: Windows PowerShell 5.1 adds CRLF by
  default, and -Encoding UTF8 adds a byte-order mark. GNU `sha256sum -c` then
  reports "No such file" because the trailing \r becomes part of the filename
  (measured in 72-01-SHA256SUM-CROSSCHECK.txt).

  NAMES. Every asset must be a bare filename that exists directly in -Dir. A name
  containing / \ or : or equal to . or .. is refused (exit 1) -- the sums file
  must never be able to point outside the directory it describes.

  EXIT CODES. 0 written; 1 an asset is missing or its name is refused;
  2 harness fault (HARNESS: prefix), e.g. -Dir absent.

  NOTE: pure ASCII only -- PowerShell 5.1 reads a UTF-8 (no BOM) .ps1 as ANSI, so
  non-ASCII punctuation corrupts parsing. Keep it ASCII.

.PARAMETER Dir
  The directory holding the assets. SHA256SUMS.txt is written into it.

.PARAMETER Assets
  Bare filenames to list, in order. Accepts an array or ONE comma-joined string
  (`powershell -File ... -Assets a,b` binds the single string 'a,b').
  Default: Rudis-win-Setup.exe, Rudis-win-Portable.zip.

.PARAMETER OutFile
  The sums file name inside -Dir. Default: SHA256SUMS.txt.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release/write-release-checksums.ps1 -Dir dist/release-10.0.0
#>
param(
  [Parameter(Position = 0)][string]$Dir = '',
  [string[]]$Assets = @('Rudis-win-Setup.exe', 'Rudis-win-Portable.zip'),
  [string]$OutFile = 'SHA256SUMS.txt'
)

$ErrorActionPreference = 'Stop'

function Say([string]$Text) { Write-Host $Text }

if (-not $Dir -or -not (Test-Path -LiteralPath $Dir -PathType Container)) {
  Say ("HARNESS: -Dir is empty or not a directory: '" + $Dir + "'")
  exit 2
}
$dirFull = (Resolve-Path -LiteralPath $Dir).Path

$list = @($Assets | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
if ($list.Count -eq 0) {
  Say "HARNESS: -Assets is empty"
  exit 2
}

$bad = $false
foreach ($name in $list) {
  if ($name -match '[\\/:]' -or $name -eq '.' -or $name -eq '..') {
    Say ("REFUSED  " + $name + "  (not a bare filename)")
    $bad = $true
    continue
  }
  if (-not (Test-Path -LiteralPath (Join-Path $dirFull $name) -PathType Leaf)) {
    Say ("MISSING  " + $name + "  (not found in -Dir)")
    $bad = $true
  }
}
if ($bad) {
  Say "RESULT: FAIL -- SHA256SUMS.txt NOT written"
  exit 1
}

$lines = @()
foreach ($name in $list) {
  $hash = (Get-FileHash -LiteralPath (Join-Path $dirFull $name) -Algorithm SHA256).Hash.ToLowerInvariant()
  $line = $hash + '  ' + $name
  $lines += $line
  Say $line
}

$outPath = Join-Path $dirFull $OutFile
[System.IO.File]::WriteAllText($outPath, (($lines -join "`n") + "`n"), (New-Object System.Text.ASCIIEncoding))
Say ("WROTE: " + $outPath + " (" + $lines.Count + " entries, LF, ASCII, no BOM)")
exit 0
