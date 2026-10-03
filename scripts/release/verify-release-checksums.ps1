<#
.SYNOPSIS
  Verify a directory of release assets against its SHA256SUMS.txt (Phase 72, SHIP-01 rescoped, D-02).

.DESCRIPTION
  Rudis ships UNSIGNED (Phase 72, D-01); integrity is published as SHA256SUMS.txt
  (written by write-release-checksums.ps1). This script recomputes every listed
  hash and fails on ANY problem. It is the pre-upload gate at the end of
  build-release.ps1 and the proof run against assets downloaded back from GitHub.

  STANDING RULE (D-02): this checker was watched FAILING on one flipped byte
  (and on a missing file, a malformed line, an empty file, a path-bearing name and
  a byte-order mark) before it was ever trusted green -- see
  72-01-CHECKER-RED-GREEN.txt. -SelfTest replays all of those arms on every run.

  PARSING. The sums file is read as BYTES. A leading EF BB BF (BOM) is a failure:
  the writer never emits one and `sha256sum -c` would choke on it. Lines are split
  on LF; ONE trailing CR per line is tolerated on read (and counted in a format:
  note) even though `sha256sum -c` rejects CRLF -- the writer emits LF only.
  Every non-empty line must match
    ^([0-9a-f]{64})  ([^\\/:*?"<>|]+)$
  (lowercase hex, two spaces, a bare filename). The names . and .. and any name
  whose GetFileName() differs from itself are also MALFORMED. A malformed entry is
  NEVER hashed, so a hostile sums file cannot make this script read anything
  outside -Dir.

  OUTPUT. Per entry: "OK        <name>" | "MISMATCH  <name>  listed=<hex> actual=<hex>"
  | "MISSING   <name>" | "MALFORMED line <n>: <text>" | "NOT LISTED  <name>" (-Only).
  Informational only: "unlisted: <name>" for files in -Dir that no line names (the
  build directory legitimately holds the nupkg and manifests), and
  "format: <k> line(s) had CRLF (tolerated on read; the writer emits LF)".

  EXIT CODES. 0 = RESULT: PASS; 1 = RESULT: FAIL; 2 = harness fault (HARNESS:
  prefix) or SELFTEST: FAIL.

  NOTE: pure ASCII only -- PowerShell 5.1 reads a UTF-8 (no BOM) .ps1 as ANSI, so
  non-ASCII punctuation corrupts parsing. Keep it ASCII.

.PARAMETER Dir
  The directory holding the assets and the sums file.

.PARAMETER SumsFile
  The sums file name inside -Dir. Default: SHA256SUMS.txt.

.PARAMETER Only
  Verify only these entries (array or ONE comma-joined string). A name given here
  that no line lists is a failure (NOT LISTED). Default: verify every entry.

.PARAMETER SelfTest
  Build synthetic fixtures under %TEMP%, replay every RED arm (tamper, missing,
  malformed, empty, traversal, BOM) and the GREEN arms through this script's own
  production entry point (a child powershell.exe -File), print SELFTEST: PASS
  (exit 0) or SELFTEST: FAIL (exit 2). The fixture root is always removed.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release/verify-release-checksums.ps1 -Dir dist/release-10.0.0

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release/verify-release-checksums.ps1 -Dir dist/release-10.0.0 -Only Rudis-win-Setup.exe,Rudis-win-Portable.zip

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release/verify-release-checksums.ps1 -SelfTest
#>
param(
  [Parameter(Position = 0)][string]$Dir = '',
  [string]$SumsFile = 'SHA256SUMS.txt',
  [string[]]$Only = @(),
  [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'

function Say([string]$Text) { Write-Host $Text }

function Invoke-SelfTest {
  # Every arm runs THIS script as a child powershell.exe -File (the production
  # entry point), never an in-process call, and reads the child's exit code.
  $root = Join-Path $env:TEMP ('rudis-sums-selftest-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
  $writer = Join-Path $PSScriptRoot 'write-release-checksums.ps1'
  $script:allPass = $true
  $ascii = New-Object System.Text.ASCIIEncoding

  function Invoke-Child([string]$Path, [string[]]$ArgList) {
    $o = & powershell -NoProfile -ExecutionPolicy Bypass -File $Path @ArgList 2>&1 | Out-String
    return @($LASTEXITCODE, $o)
  }
  function Regen {
    $r = Invoke-Child $writer @('-Dir', $root, '-Assets', 'a.bin,b.bin,c.bin')
    if ($r[0] -ne 0) { throw ("writer failed (exit " + $r[0] + "): " + $r[1]) }
  }
  function Arm([string]$Id, [string]$Label, [int]$Expect, [string]$Needle, [string[]]$Extra, [string]$MustNot = '') {
    $r = Invoke-Child $PSCommandPath (@('-Dir', $root) + $Extra)
    $has = $r[1].Contains($Needle)
    $ok = ($r[0] -eq $Expect) -and $has
    $tail = ''
    if ($MustNot) {
      $absent = -not $r[1].Contains($MustNot)
      $ok = $ok -and $absent
      $tail = (" lacks '" + $MustNot + "'=" + $absent)
    }
    $verdict = $(if ($ok) { 'PASS' } else { 'FAIL' })
    if (-not $ok) { $script:allPass = $false }
    Say ("ARM " + $Id + " " + $Label + ": expected exit=" + $Expect + " got=" + $r[0] +
         " contains '" + $Needle + "'=" + $has + $tail + " [" + $verdict + "]")
    if (-not $ok) { foreach ($l in ($r[1] -split "`r?`n")) { if ($l) { Say ("    | " + $l) } } }
  }

  Say "SELFTEST: verify-release-checksums.ps1 (arms run through powershell -File, RED before GREEN)"
  try {
    New-Item -ItemType Directory -Path $root | Out-Null
    $rng = New-Object System.Random(7201)
    foreach ($pair in @(@('a.bin', 1024), @('b.bin', 65536), @('c.bin', 3))) {
      $b = New-Object byte[] $pair[1]; $rng.NextBytes($b)
      [System.IO.File]::WriteAllBytes((Join-Path $root $pair[0]), $b)
    }
    $sums = Join-Path $root 'SHA256SUMS.txt'
    Regen

    $bp = Join-Path $root 'b.bin'
    $bb = [System.IO.File]::ReadAllBytes($bp); $bb[10] = $bb[10] -bxor 1; [System.IO.File]::WriteAllBytes($bp, $bb)
    Arm 1 'tamper (byte 10 of b.bin flipped)' 1 'MISMATCH  b.bin' @()
    $bb[10] = $bb[10] -bxor 1; [System.IO.File]::WriteAllBytes($bp, $bb)

    Rename-Item -LiteralPath (Join-Path $root 'c.bin') -NewName 'c.bin.away'
    Arm 2 'missing (c.bin renamed)' 1 'MISSING   c.bin' @()
    Rename-Item -LiteralPath (Join-Path $root 'c.bin.away') -NewName 'c.bin'

    $txt = [System.IO.File]::ReadAllText($sums)
    [System.IO.File]::WriteAllText($sums, ('g' + $txt.Substring(1)), $ascii)
    Arm 3 'malformed (first char -> g)' 1 'MALFORMED line 1' @()
    Regen

    [System.IO.File]::WriteAllText($sums, '', $ascii)
    Arm 4 'empty sums file' 1 'no entries' @()
    Regen

    $txt = [System.IO.File]::ReadAllText($sums)
    [System.IO.File]::WriteAllText($sums, ($txt + ('0' * 64) + '  ..\..\evil.bin' + "`n"), $ascii)
    Arm 5 'traversal line appended' 1 'MALFORMED line 4' @() 'MISSING'
    Regen

    $gb = [System.IO.File]::ReadAllBytes($sums)
    [System.IO.File]::WriteAllBytes($sums, ([byte[]](0xEF, 0xBB, 0xBF) + $gb))
    Arm 6 'byte-order mark prefix' 1 'byte-order mark' @()
    Regen

    $txt = [System.IO.File]::ReadAllText($sums)
    [System.IO.File]::WriteAllText($sums, ($txt -replace "`n", "`r`n"), $ascii)
    Arm 7 'CRLF good file (tolerated)' 0 'format: 3 line(s) had CRLF' @()
    Regen

    Arm 8 '-Only a.bin' 0 'RESULT: PASS -- 1 file(s) verified' @('-Only', 'a.bin')
    Arm 8 '-Only zzz.bin' 1 'NOT LISTED  zzz.bin' @('-Only', 'zzz.bin')

    Arm 9 'untampered' 0 'RESULT: PASS -- 3 file(s) verified' @()
  }
  catch {
    Say ("HARNESS: selftest fixture error: " + $_.Exception.Message)
    $script:allPass = $false
  }
  finally {
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
    Say ("selftest-root-removed: " + (-not (Test-Path -LiteralPath $root)))
  }

  if ($script:allPass) { Say "SELFTEST: PASS"; exit 0 }
  Say "SELFTEST: FAIL"
  exit 2
}

if ($SelfTest) { Invoke-SelfTest }

if (-not $Dir -or -not (Test-Path -LiteralPath $Dir -PathType Container)) {
  Say ("HARNESS: -Dir is empty or not a directory: '" + $Dir + "'")
  exit 2
}
$dirFull  = (Resolve-Path -LiteralPath $Dir).Path
$sumsPath = Join-Path $dirFull $SumsFile
$onlyList = @($Only | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })

Say ("sums: " + $sumsPath)
Say ("dir : " + $Dir)

$problems = 0
function Finish([int]$Verified) {
  if ($script:problems -eq 0) {
    Say ("RESULT: PASS -- " + $Verified + " file(s) verified")
    exit 0
  }
  Say ("RESULT: FAIL -- " + $script:problems + " problem(s)")
  exit 1
}

if (-not (Test-Path -LiteralPath $sumsPath -PathType Leaf)) {
  Say ("sums file not found: " + $SumsFile)
  $problems++
  Finish 0
}

$bytes = [System.IO.File]::ReadAllBytes($sumsPath)
if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
  Say "MALFORMED line 1: byte-order mark (EF BB BF) -- the writer never emits one"
  $problems++
  Finish 0
}

$text = (New-Object System.Text.ASCIIEncoding).GetString($bytes)
$raw  = $text -split "`n"
$crCount = 0
$entries = @()
$lineRe  = '^([0-9a-f]{64})  ([^\\/:*?"<>|]+)$'
for ($i = 0; $i -lt $raw.Count; $i++) {
  $line = $raw[$i]
  if ($line.EndsWith("`r")) { $line = $line.Substring(0, $line.Length - 1); $crCount++ }
  if ($line -eq '') { continue }
  $n = $i + 1
  if ($line -cmatch $lineRe) {
    $hex  = $Matches[1]
    $name = $Matches[2]
    if ($name -eq '.' -or $name -eq '..' -or [System.IO.Path]::GetFileName($name) -ne $name) {
      Say ("MALFORMED line " + $n + ": " + $line)
      $problems++
      continue
    }
    $entries += , @($hex, $name, $n)
  } else {
    Say ("MALFORMED line " + $n + ": " + $line)
    $problems++
  }
}

if ($crCount -gt 0) {
  Say ("format: " + $crCount + " line(s) had CRLF (tolerated on read; the writer emits LF)")
}

if ($entries.Count -eq 0 -and $problems -eq 0) {
  Say ("no entries in " + $SumsFile)
  $problems++
  Finish 0
}

$verified = 0
$listedNames = @()
foreach ($e in $entries) {
  $hex = $e[0]; $name = $e[1]
  $listedNames += $name
  if ($onlyList.Count -gt 0 -and ($onlyList -notcontains $name)) { continue }
  $full = Join-Path $dirFull $name
  if (-not (Test-Path -LiteralPath $full -PathType Leaf)) {
    Say ("MISSING   " + $name)
    $problems++
    continue
  }
  $actual = (Get-FileHash -LiteralPath $full -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($actual -ieq $hex) {
    Say ("OK        " + $name)
    $verified++
  } else {
    Say ("MISMATCH  " + $name + "  listed=" + $hex + " actual=" + $actual)
    $problems++
  }
}

foreach ($o in $onlyList) {
  if ($listedNames -notcontains $o) {
    Say ("NOT LISTED  " + $o)
    $problems++
  }
}

foreach ($f in @(Get-ChildItem -LiteralPath $dirFull -File)) {
  if ($f.Name -eq $SumsFile) { continue }
  if ($listedNames -notcontains $f.Name) { Say ("unlisted: " + $f.Name) }
}

Finish $verified
