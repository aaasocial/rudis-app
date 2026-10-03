# Rudis — Notices

Copyright (C) 2026 The Rudis Authors

The copyright holder is the project owner, reachable through the repository's GitHub account.

This program is free software: you can redistribute it and/or modify it under the terms of the
GNU Affero General Public License, **version 3**, as published by the Free Software Foundation.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without
even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
Affero General Public License for more details.

You should have received a copy of the GNU Affero General Public License along with this program;
it is the file `LICENSE` at the root of this repository (the verbatim FSF text — never edit it).
If not, see <https://www.gnu.org/licenses/>.

**SPDX identifier: `AGPL-3.0-only`.** Version 3 only, not "or any later version": the owner holds
the copyright, so no future licence text published by anyone else can change the terms without
the owner choosing it, and a commercial dual-licence remains available to the copyright holder.
Every first-party Cargo crate (`[workspace.package] license`) and the shipped C# project
(`PackageLicenseExpression`) carry the same identifier.

## You may earn money with Rudis

Using Rudis for paid work, client work or commercial content is **unrestricted**. The videos you
make are yours; the licence places no condition on them and takes no share of them.

What the AGPL governs is Rudis itself: if you distribute a modified version of Rudis — or let
people use a modified version over a network — you must release that modified version under this
same licence, with its complete source code. Nobody can take Rudis and sell it as a closed,
proprietary product.

The copyright holder may offer a commercial dual-licence later. Nothing in this notice promises one.

## FFmpeg

Rudis uses FFmpeg under the **GNU Lesser General Public License (LGPL), version 3 or later**. The
builds are configured with `--enable-version3`, without `--enable-gpl` or `--enable-nonfree`, and
with x264/x265 disabled. The LGPL is compatible with the AGPL-3.0, and in both of the ways Rudis
uses FFmpeg, FFmpeg keeps its own licence:

1. **CLI sidecar.** `ffmpeg.exe` / `ffprobe.exe` in `<app>\binaries\` run as separate subprocesses
   (mere aggregation). They decode for thumbnails, waveforms and proxies, and do all export
   encoding (`PROVENANCE.md` Entry 5).
2. **Shared libraries.** `avcodec-62.dll`, `avutil-60.dll` and the rest of that set ship beside
   `Rudis.Shell.exe`. `rudis_ffi.dll` links them at load time for hardware-accelerated preview
   decoding (`PROVENANCE.md` Entry 21). They are the standard shared builds, and a user can
   replace them.

No FFmpeg source is copied into this repository. Sources, pinned download URLs and SHA-256 hashes
are in `PROVENANCE.md` and in the two fetch scripts under `scripts/windows/`.

## Patent notice — three encoders in the second FFmpeg payload (D-62-01-01)

The sidecar build (`N-125907`, `runtime/binaries/`) had three encoders removed on 2026-08-30
(SHIP-04): `libopenh264`, `libkvazaar` and `libvvenc`.

The **second** payload is `crates/engine/ffmpeg-dev/` (`n8.0.1-48-g0592be14ff`, `avcodec-62` /
`avutil-60`). Its DLLs ship beside `Rudis.Shell.exe`, and it **still contains those three
encoders**. On 2026-09-29, its `ffmpeg.exe -encoders` listed all three (count 3), and
`avcodec-62.dll` contains their names. On the sidecar, `ffmpeg.exe -encoders` lists none (count 0);
the only place those names appear in `avcodec-63.dll` is its configure string, as `--disable-`
flags.

Rudis never selects these encoders:

- Export runs `h264_mf` or NVENC through the sidecar.
- The in-process library path only decodes.
- That payload's `.exe` files are not staged into the shipped app, so the app has no command line
  that could reach the three encoders.

**Copyright licensing and patent licensing are different axes. The AGPL governs the copyright in
this code; it grants no licence under any H.264/AVC, HEVC or VVC patent and does not resolve codec
patent exposure — for the encoders named here or for the decoders the product does use. Anyone
distributing Rudis in a jurisdiction that recognises software patents must assess that exposure
themselves.**

The owner has an open follow-up (`D-62-01-01`): rebuild the second payload without the three
encoders. When that lands, update this notice and the markers below together.

Aside: a `dist/Rudis/binaries/ffmpeg.exe` dated `20260802` on a developer machine is the
rollback copy from before the removal. It is not a build artifact of this tree.

<!-- payload-claim: runtime/binaries/ffmpeg.exe libopenh264,libkvazaar,libvvenc = ABSENT -->
<!-- payload-claim: runtime/binaries/avcodec-63.dll libopenh264,libkvazaar,libvvenc = ABSENT -->
<!-- payload-claim: crates/engine/ffmpeg-dev/bin/ffmpeg.exe libopenh264,libkvazaar,libvvenc = PRESENT -->
<!-- payload-claim: crates/engine/ffmpeg-dev/bin/avcodec-62.dll libopenh264,libkvazaar,libvvenc = PRESENT -->

## Other bundled third-party components

| Component | Licence | Ships? | PROVENANCE |
|---|---|---|---|
| whisper.cpp CLI + `whisper.dll` / `ggml*.dll` | MIT | yes (sidecar) | Entry 11 |
| `ggml-small.bin` Whisper model weights | MIT | yes | Entry 11 |
| `SDL2.dll` (SDL 2.28.5) | Zlib | yes (arrives inside the whisper.cpp release zip; not loaded by Rudis) | Entry 40 |
| `parakeet.dll` (whisper.cpp v1.9.1's Parakeet speech-model library) | MIT | yes (same release zip; not loaded by Rudis) | Entry 40 |
| Python 3.12 embeddable | PSF-2.0 | yes (tracker sidecar) | Entry 12 |
| `opencv-contrib-python` | MIT wrapper over Apache-2.0 OpenCV | yes (tracker sidecar) | Entry 12 |
| `numpy` | BSD-3-Clause | yes (tracker sidecar) | Entry 12 |
| FFmpeg inside `opencv-contrib-python` (`opencv_videoio_ffmpeg500_64.dll`) | LGPL-2.1 | yes (tracker sidecar; loaded by OpenCV, not by Rudis) | Entry 40 |
| Microsoft Visual C++ runtime inside the tracker sidecar (`vcruntime140*.dll`, `msvcp140-*.dll`) | Visual Studio redistributable terms (proprietary) — **accepted as an AGPL-3.0 §1 System Library** (compiler runtime; owner decision 2026-09-30, same as below) | yes (tracker sidecar) | Entry 40 |
| Inter font | OFL-1.1 | yes | Entry 10 |
| Windows App SDK / WinUI 3 runtime | Microsoft Software License Terms (proprietary, redistribution granted) — **accepted as an AGPL-3.0 §1 System Library** (owner decision 2026-09-30, see below) | yes (self-contained) | Entry 23 |
| `Microsoft.Web.WebView2` loader | BSD-3-Clause | yes | Entry 23 |
| Velopack | MIT | yes (installer + updater) | Entry 38 |
| .NET 9 runtime | MIT | yes (self-contained) | Entry 40 |

**Windows App SDK / WinUI 3 runtime and the Visual C++ runtime: accepted as System Libraries.**
The copyright holder's position (2026-09-30) is the usual reading: a platform UI runtime
distributed out-of-band by the OS vendor, and a compiler runtime, fall under AGPL-3.0 §1's System
Libraries carve-out — the posture every (A)GPL WPF/WinUI/.NET desktop app takes. No §7 additional
permission is added. Both remain proprietary Microsoft components and are listed on every run of
the licence gate.

`THIRD-PARTY-LICENSES.md` reconciles every shipped crate, package and binary. It is generated and
checked by `scripts/oss/check-licenses.ps1`. The provenance ledger is `PROVENANCE.md`.

## What this notice does not do

- It grants no trademark rights in the name "Rudis" or its logos.
- It grants no patent licence beyond the contributor patent grant in AGPL-3.0 §11, which covers
  Rudis's own code.
- It says nothing about the licences of the media a user edits with Rudis.
