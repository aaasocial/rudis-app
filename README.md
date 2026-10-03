# Rudis

Rudis is a Windows desktop video editor for people with no editing experience. You import a real
video file, preview real decoded frames, arrange it on a timeline (trim, split, rearrange), adjust
the audio (volume, detach), and export a real file to disk that reflects every edit. An optional
in-app agent and a Canvas (a whiteboard you can sketch on) drive the same real timeline as the
standard editing tools. Rudis is licensed **AGPL-3.0-only**. It runs on Windows only: a native
WinUI 3 shell over a Rust engine, with an LGPL build of FFmpeg run as a separate sidecar process.

Tested on Windows 10/11 x64.

## Download and install

Prebuilt binaries are on the Releases page of this repository. Each release carries:

| Asset | What it is |
|-------|------------|
| `Rudis-win-Setup.exe` | The installer. Per-user (`%LOCALAPPDATA%\Rudis`), no administrator rights; it adds an *Apps & features* entry named `Rudis` for uninstalling. |
| `Rudis-win-Portable.zip` | The same app without an installer: unzip anywhere and run `Rudis.exe`. |
| `SHA256SUMS.txt` | One line per file above: its SHA-256 hash and its name, in `sha256sum` format. |

**The binaries are not code-signed.** Rudis is open source and has no paid code-signing
certificate, so Windows SmartScreen shows **"Windows protected your PC"** the first time you run
`Rudis-win-Setup.exe`. Click **More info**, then **Run anyway**. The warning is about the missing
signature, not about what the installer does: it installs for your user only and asks for no
administrator rights.

**Verify what you downloaded.** Compare each file's hash with its line in `SHA256SUMS.txt` from the
same release. In Windows PowerShell, in the folder you downloaded to:

```powershell
Get-FileHash .\Rudis-win-Setup.exe -Algorithm SHA256
```

The `Hash` it prints must equal, ignoring letter case, the first field of the `Rudis-win-Setup.exe`
line in `SHA256SUMS.txt`. In Git Bash or WSL, with the downloads and `SHA256SUMS.txt` in one folder,
`sha256sum -c SHA256SUMS.txt` checks every file at once.

A matching hash proves the file arrived intact and is the one the release lists. It does not prove
who built it: `SHA256SUMS.txt` is published next to the files it describes, so whoever could replace
one could replace both. **Building from source (below) is the fully trusted route.** A free
open-source signing programme such as SignPath Foundation is a possible future option; nothing is
planned.

## Prerequisites

This is what you install by hand. The next section verifies each item and fails by name if one is
missing.

| Tool | Version | Why | Install |
|------|---------|-----|---------|
| Windows | Windows 10 1809+ or Windows 11, x64, with a **D3D12-capable GPU** | The preview and hardware decode run on the GPU through Direct3D 12 | - |
| **Git** | any recent | Clone the repository | git-scm.com |
| **Visual Studio 2022 Build Tools** | *Desktop development with C++* workload (MSVC v143 + a Windows 10/11 SDK) | rustc's linker and the C parts of the Rust build need them; the Community/Professional editions with the same workload also work | visualstudio.microsoft.com |
| **.NET SDK** | **9.0.316 or any later 9.0 SDK** (a later 9.0.3xx, or a later feature band such as 9.0.4xx) | Builds the WinUI 3 shell; `global.json` pins 9.0.316 with `rollForward: latestFeature` | dotnet.microsoft.com |
| **Rust (stable) via rustup** | `stable` (`rust-toolchain.toml`); last verified with rustc 1.96.1 | Builds the engine and the native library the shell loads | rustup.rs |

You also need about 10 GB of free disk and a network connection for the build (crates.io, NuGet,
and the three pinned downloads below). Clone to a SHORT path such as `C:\src\Rudis`: Windows'
260-character path limit bites deep build trees.

```text
git clone <repository-url> Rudis
cd Rudis
```

## Build from source

Run every block below in **Windows PowerShell** from the repository root, top to bottom. Each block
is copy-paste; nothing in this section is optional.

### 1. Check the prerequisites

```powershell
git --version
dotnet --version
cargo --version
rustc --version
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'Visual Studio 2022 Build Tools with the "Desktop development with C++" workload was not found' }
Write-Host "MSVC toolchain: $vs"
```

What a failure of each line means:

- `git --version` fails: Git is not installed or not on `PATH`.
- `dotnet --version` fails inside the clone: no .NET SDK at 9.0.316 or later within 9.0 is installed (`global.json` requires one; an older 9.0 SDK or a different major version such as 8.0 or 10.0 is not enough).
- `cargo --version` / `rustc --version` fail: Rust is not installed through rustup, or a new terminal is needed after installing it.
- The `vswhere.exe` line fails, or the next line throws: Visual Studio 2022 (or its Build Tools) is missing, or it lacks the *Desktop development with C++* workload.

### 2. Fetch the pinned payloads

Three downloads, about 175 MB in total. Each is SHA256-pinned by its script: the scripts refuse a
file whose hash does not match and refuse a GPL build of FFmpeg. Run them as-is; do not pass
alternative URLs or hashes.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\fetch-libclang18.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\fetch-ffmpeg-devlibs.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\fetch-lgpl-ffmpeg.ps1
```

- `fetch-libclang18.ps1` installs libclang 18.1.1 into `crates\engine\libclang\` (used by the Rust build's bindgen; without it the build stops and names this script).
- `fetch-ffmpeg-devlibs.ps1` installs the FFmpeg 8.0.1 LGPL development libraries into `crates\engine\ffmpeg-dev\` (the hardware-decode preview path links them).
- `fetch-lgpl-ffmpeg.ps1` installs the LGPL FFmpeg sidecar (`ffmpeg.exe`, `ffprobe.exe` and their DLLs) into `runtime\binaries\` (what the app runs for decode, encode and export; published beside the exe).

### 3. Build and publish

This compiles the Rust workspace too (the project files invoke cargo), restores NuGet packages, and
publishes a self-contained app into `dist\Rudis` inside the clone. A cold build takes tens of
minutes and downloads every crate once.

Why publish and not build: only `dotnet publish` stages the FFmpeg sidecar beside the exe. A plain
`dotnet build` output has none, and the engine would fall back to whatever `ffmpeg` is on `PATH`
(or find none).

```powershell
dotnet publish shell\Rudis.Shell\Rudis.Shell.csproj -c Release -p:Platform=x64 -o dist\Rudis
```

### 4. Run it

```powershell
Start-Process -FilePath .\dist\Rudis\Rudis.Shell.exe
```

## Run

After the first build, launch `dist\Rudis\Rudis.Shell.exe` directly. After pulling changes, re-run
step 3 of the build.

The app stores projects under `%APPDATA%\app.rudis.desktop\projects` and caches under
`%LOCALAPPDATA%\app.rudis.desktop`.

As an optional convenience, this script publishes the app and creates a desktop shortcut:

```text
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\install-desktop-shortcut.ps1
```

It also links your repository `.env` (which may hold plaintext API keys) into `dist\Rudis`
beside the exe, as a hardlink, or as a copy if a hardlink is not possible. Do not zip or share
`dist\Rudis` as-is after running it.

No keys, no account and no network are needed to import, edit and export.

## API keys (optional)

The editor works fully with no key at all: import, edit and export, offline. Two optional features
take keys: the **agent** (Anthropic) and **generation** (Runway).

- Enter them in **Settings**: the TitleBar app-glyph menu -> *Settings...*, `Ctrl+,`, or the key
  button in the Chat panel. Keys are held by **Windows Credential Manager** (targets
  `anthropic-api-key.rudis` and `gen-runway-api-key.rudis`). Save, Replace and Clear take effect
  immediately; no restart.
- Precedence: Credential Manager first, then the process environment (`ANTHROPIC_API_KEY`;
  `RUNWAY_API_KEY`, falling back to `RUNWAYML_API_SECRET`; `ELEVENLABS_API_KEY`), then none.
- A `.env` file in the repository root (or beside the exe) is a **developer convenience**: the app
  loads it at startup only for variables that are not already set. `dotnet publish` never
  includes it (only the optional shortcut script above places it beside the exe).
  `RUDIS_NO_DOTENV=1` disables it.
- ElevenLabs (voice) has no Settings UI; set `ELEVENLABS_API_KEY` (environment or `.env`). A
  `gen-elevenlabs-api-key.rudis` Credential Manager entry, if present, takes precedence over it.
- The Chat panel shows `connected` or `unavailable — <why>`.

Keys are obtained from console.anthropic.com (Anthropic) and dev.runwayml.com (Runway).

## Optional features

These are NOT part of the build above:

```text
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\fetch-whisper-cli.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\fetch-opencv-sdk.ps1
```

- `fetch-whisper-cli.ps1` installs whisper-cli and the ggml-small model: offline transcription,
  remove-words editing and subtitles.
- `fetch-opencv-sdk.ps1` installs the OpenCV sidecar (an embeddable Python with OpenCV): motion
  tracking.

Without them, those features report themselves unavailable; everything else works. Both are
hundreds of MB. Run them BEFORE step 3, or re-run step 3 afterwards, because publish copies
`runtime\binaries` into `dist\Rudis\binaries`.

## Known limitations

- The release binaries are not code-signed, so Windows SmartScreen warns on the first run of each
  new release. See *Download and install* above for the *More info* -> *Run anyway* path and how
  to verify the download.

## Troubleshooting

- **The third fetch (`fetch-lgpl-ffmpeg.ps1`) fails with 404.** It downloads a pinned release from
  a GitHub fork of FFmpeg-Builds; a 404 means the pinned release moved. Open an issue; do not point
  the script elsewhere.
- **The app exits immediately with code `-1073741189` and no window.** The publish is incomplete
  (a missing `Rudis.Shell.pri` or native DLLs). Re-run step 3.
- **Path-too-long errors from cargo or MSBuild.** Clone to a shorter path such as `C:\src\Rudis`.
- **`dotnet --version` errors inside the clone.** Install .NET SDK 9.0.316 or any later 9.0 SDK.
- **No window, or a black preview.** The GPU must support Direct3D 12.

## Licence

Rudis is **AGPL-3.0-only**. `LICENSE` is the verbatim FSF text; `NOTICE.md` carries the notices,
including that using Rudis for paid work is unrestricted (what the AGPL governs is distributing
modified Rudis). `THIRD-PARTY-LICENSES.md` records every third-party part.

FFmpeg is an **LGPL v3** build run as a separate sidecar process, never a GPL build. H.264/HEVC
export uses hardware / Windows Media Foundation encoders.
