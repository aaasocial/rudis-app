# Third-party licences -- Rudis

<!-- generated 2026-09-30T21:10:52Z from HEAD 6db2f98933a0a19c8cc3160414aad11d4d4e57ed by: powershell -NoProfile -ExecutionPolicy Bypass -File scripts/oss/check-licenses.ps1 -Report THIRD-PARTY-LICENSES.md -->

Rudis is licensed under the **GNU Affero General Public License, version 3 only** (`AGPL-3.0-only`).
The licence text is `LICENSE` (the verbatim FSF text); the notices -- including the FFmpeg LGPL
posture and the codec-patent notice -- are in `NOTICE.md`; where every third-party part came from
is recorded in `PROVENANCE.md`.

This file is **generated**. Do not edit it by hand; change the inputs and regenerate:

```
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/oss/check-licenses.ps1 -Report THIRD-PARTY-LICENSES.md
```

The same script without `-Report` is the OSS-04 gate: it exits 1 if any row below is FAIL. It
checks the Rust closure of the two native libraries the app loads (`rudis_ffi.dll`,
`rudis_timeline.dll`), every NuGet package in the C# projects, and every bundled binary, against
the AGPL-3.0 compatibility tables in `scripts/oss/license-dispositions.json`. A licence id that is in
neither table fails as unknown. Rows marked WARN are **flags**: they are shipped, they have a written
disposition, and they are listed with their reasons in the last section -- a flag is not a clearance.

Codec patents are a separate axis: copyright licensing and patent licensing are different things,
and the AGPL grants no licence under any codec patent. See `NOTICE.md`.

**Verdict at generation: PASS** (0 FAIL, 19 WARN).

## 1. LICENSE pin

| Item | Result | Detail |
|---|---|---|
| `LICENSE` | PASS | sha256 0d96a4ff68ad6d4b6f1f30f713b18d5184912ba8dd389f86aa7710db079abcb0, 34523 bytes (CR-stripped) |
| `.gitattributes` | PASS | LICENSE -text (LICENSE: text: unset) |

## 2. Rust crates shipped in the native libraries (271)

`cargo tree -p ffi -p timeline-render -e normal --target x86_64-pc-windows-msvc`, deduplicated. First-party
workspace crates carry `AGPL-3.0-only`. "Elected" names the licence relied on where the crate offers a choice.

| Crate | Version | Licence | Result | Note |
|---|---|---|---|---|
| `adler2` | 2.0.1 | 0BSD OR MIT OR Apache-2.0 | PASS | elected 0BSD |
| `agent-gen` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `agent-llm` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `agent-tools` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `aho-corasick` | 1.1.4 | Unlicense OR MIT | PASS | elected Unlicense |
| `allocator-api2` | 0.2.21 | MIT OR Apache-2.0 | PASS | elected MIT |
| `anyhow` | 1.0.103 | MIT OR Apache-2.0 | PASS | elected MIT |
| `app-core` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `arrayvec` | 0.7.7 | MIT OR Apache-2.0 | PASS | elected MIT |
| `ash` | 0.38.0+1.3.281 | MIT OR Apache-2.0 | PASS | elected MIT |
| `atomic-waker` | 1.1.2 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `aws-lc-rs` | 1.17.1 | ISC AND (Apache-2.0 OR ISC) | PASS | elected ISC AND Apache-2.0 |
| `aws-lc-sys` | 0.42.0 | ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0) | PASS | elected ISC AND Apache-2.0 AND Apache-2.0 AND MIT AND BSD-3-Clause AND Apache-2.0 AND Apache-2.0 |
| `base64` | 0.22.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `bitflags` | 2.13.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `bit-set` | 0.8.0 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `bit-set` | 0.9.1 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `bit-vec` | 0.8.0 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `bit-vec` | 0.9.1 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `bon` | 3.9.3 | MIT OR Apache-2.0 | PASS | elected MIT |
| `bon-macros` | 3.9.3 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `bytemuck` | 1.25.0 | Zlib OR Apache-2.0 OR MIT | PASS | elected Zlib |
| `bytemuck_derive` | 1.10.2 | Zlib OR Apache-2.0 OR MIT | PASS | proc-macro (build-time); elected Zlib |
| `byteorder` | 1.5.0 | Unlicense OR MIT | PASS | elected Unlicense |
| `byteorder-lite` | 0.1.0 | Unlicense OR MIT | PASS | elected Unlicense |
| `bytes` | 1.12.0 | MIT | PASS |  |
| `cfg-if` | 1.0.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `codespan-reporting` | 0.12.0 | Apache-2.0 | PASS |  |
| `codespan-reporting` | 0.13.1 | Apache-2.0 | PASS |  |
| `composition-scale` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `core` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `core_maths` | 0.1.1 | MIT | PASS |  |
| `cosmic-text` | 0.18.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `cosmic-text` | 0.19.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `cpal` | 0.18.1 | Apache-2.0 | PASS |  |
| `crc32fast` | 1.5.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `darling` | 0.23.0 | MIT | PASS |  |
| `darling_core` | 0.23.0 | MIT | PASS |  |
| `darling_macro` | 0.23.0 | MIT | PASS | proc-macro (build-time) |
| `dasp_sample` | 0.11.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `displaydoc` | 0.2.6 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `document-features` | 0.2.12 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `encoding_rs` | 0.8.35 | (Apache-2.0 OR MIT) AND BSD-3-Clause | PASS | elected Apache-2.0 AND BSD-3-Clause |
| `engine` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `equivalent` | 1.0.2 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `etagere` | 0.3.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `euclid` | 0.22.14 | MIT OR Apache-2.0 | PASS | elected MIT |
| `fastrand` | 2.4.1 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `fdeflate` | 0.3.7 | MIT OR Apache-2.0 | PASS | elected MIT |
| `ffi` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `filmstrip` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `flate2` | 1.1.9 | MIT OR Apache-2.0 | PASS | elected MIT |
| `fnv` | 1.0.7 | Apache-2.0 / MIT | PASS | elected Apache-2.0 |
| `foldhash` | 0.1.5 | Zlib | PASS |  |
| `foldhash` | 0.2.0 | Zlib | PASS |  |
| `fontdb` | 0.23.0 | MIT | PASS |  |
| `font-types` | 0.11.3 | MIT OR Apache-2.0 | PASS | elected MIT |
| `form_urlencoded` | 1.2.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `futures-channel` | 0.3.32 | MIT OR Apache-2.0 | PASS | elected MIT |
| `futures-core` | 0.3.32 | MIT OR Apache-2.0 | PASS | elected MIT |
| `futures-sink` | 0.3.32 | MIT OR Apache-2.0 | PASS | elected MIT |
| `futures-task` | 0.3.32 | MIT OR Apache-2.0 | PASS | elected MIT |
| `futures-util` | 0.3.32 | MIT OR Apache-2.0 | PASS | elected MIT |
| `getrandom` | 0.4.3 | MIT OR Apache-2.0 | PASS | elected MIT |
| `glow` | 0.16.0 | MIT OR Apache-2.0 OR Zlib | PASS | elected MIT |
| `glow` | 0.17.0 | MIT OR Apache-2.0 OR Zlib | PASS | elected MIT |
| `glutin_wgl_sys` | 0.6.1 | Apache-2.0 | PASS |  |
| `glyphon` | 0.11.0 | MIT OR Apache-2.0 OR Zlib | PASS | elected MIT |
| `gpu-alloc` | 0.6.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `gpu-allocator` | 0.27.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `gpu-allocator` | 0.28.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `gpu-alloc-types` | 0.3.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `gpu-descriptor` | 0.3.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `gpu-descriptor-types` | 0.2.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `h2` | 0.4.15 | MIT | PASS |  |
| `half` | 2.7.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `harfrust` | 0.5.2 | MIT | PASS |  |
| `hashbrown` | 0.15.5 | MIT OR Apache-2.0 | PASS | elected MIT |
| `hashbrown` | 0.16.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `hashbrown` | 0.17.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `hexf-parse` | 0.2.1 | CC0-1.0 | PASS |  |
| `http` | 1.4.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `httparse` | 1.10.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `http-body` | 1.0.1 | MIT | PASS |  |
| `http-body-util` | 0.1.3 | MIT | PASS |  |
| `hyper` | 1.10.1 | MIT | PASS |  |
| `hyper-rustls` | 0.27.9 | Apache-2.0 OR ISC OR MIT | PASS | elected Apache-2.0 |
| `hyper-util` | 0.1.20 | MIT | PASS |  |
| `icu_collections` | 2.2.0 | Unicode-3.0 | PASS |  |
| `icu_locale_core` | 2.2.0 | Unicode-3.0 | PASS |  |
| `icu_normalizer` | 2.2.0 | Unicode-3.0 | PASS |  |
| `icu_normalizer_data` | 2.2.0 | Unicode-3.0 | PASS |  |
| `icu_properties` | 2.2.0 | Unicode-3.0 | PASS |  |
| `icu_properties_data` | 2.2.0 | Unicode-3.0 | PASS |  |
| `icu_provider` | 2.2.0 | Unicode-3.0 | PASS |  |
| `ident_case` | 1.0.1 | MIT/Apache-2.0 | PASS | elected MIT |
| `idna` | 1.1.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `idna_adapter` | 1.2.2 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `image` | 0.25.10 | MIT OR Apache-2.0 | PASS | elected MIT |
| `indexmap` | 2.14.0 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `ipnet` | 2.12.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `itoa` | 1.0.18 | MIT OR Apache-2.0 | PASS | elected MIT |
| `keyring` | 4.1.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `keyring-core` | 1.0.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `khronos-egl` | 6.0.0 | MIT/Apache-2.0 | PASS | elected MIT |
| `libc` | 0.2.186 | MIT OR Apache-2.0 | PASS | elected MIT |
| `libloading` | 0.8.9 | ISC | PASS |  |
| `libm` | 0.2.16 | MIT | PASS |  |
| `linebender_resource_handle` | 0.1.1 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `linkme` | 0.3.37 | MIT OR Apache-2.0 | PASS | elected MIT |
| `linkme-impl` | 0.3.37 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `litemap` | 0.8.2 | Unicode-3.0 | PASS |  |
| `litrs` | 1.0.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `lock_api` | 0.4.14 | MIT OR Apache-2.0 | PASS | elected MIT |
| `log` | 0.4.33 | MIT OR Apache-2.0 | PASS | elected MIT |
| `lru` | 0.16.4 | MIT | PASS |  |
| `memchr` | 2.8.2 | Unlicense OR MIT | PASS | elected Unlicense |
| `memmap2` | 0.9.11 | MIT OR Apache-2.0 | PASS | elected MIT |
| `mime` | 0.3.17 | MIT OR Apache-2.0 | PASS | elected MIT |
| `mime_guess` | 2.0.5 | MIT | PASS |  |
| `miniz_oxide` | 0.8.9 | MIT OR Zlib OR Apache-2.0 | PASS | elected MIT |
| `mio` | 1.2.1 | MIT | PASS |  |
| `moxcms` | 0.8.1 | BSD-3-Clause OR Apache-2.0 | PASS | elected BSD-3-Clause |
| `naga` | 26.0.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `naga` | 29.0.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `num-traits` | 0.2.19 | MIT OR Apache-2.0 | PASS | elected MIT |
| `once_cell` | 1.21.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `ordered-float` | 5.0.0 | MIT | PASS |  |
| `parking_lot` | 0.12.5 | MIT OR Apache-2.0 | PASS | elected MIT |
| `parking_lot_core` | 0.9.12 | MIT OR Apache-2.0 | PASS | elected MIT |
| `paste` | 1.0.15 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `percent-encoding` | 2.3.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `pin-project-lite` | 0.2.17 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `png` | 0.18.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `pollster` | 0.4.0 | Apache-2.0/MIT | PASS | elected Apache-2.0 |
| `potential_utf` | 0.1.5 | Unicode-3.0 | PASS |  |
| `presser` | 0.3.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `prettyplease` | 0.2.37 | MIT OR Apache-2.0 | PASS | elected MIT |
| `preview` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `proc-macro2` | 1.0.106 | MIT OR Apache-2.0 | PASS | elected MIT |
| `profiling` | 1.0.18 | MIT OR Apache-2.0 | PASS | elected MIT |
| `proxy` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `pxfm` | 0.1.29 | BSD-3-Clause OR Apache-2.0 | PASS | elected BSD-3-Clause |
| `quote` | 1.0.46 | MIT OR Apache-2.0 | PASS | elected MIT |
| `range-alloc` | 0.1.5 | MIT OR Apache-2.0 | PASS | elected MIT |
| `rangemap` | 1.7.1 | MIT/Apache-2.0 | PASS | elected MIT |
| `raw-window-handle` | 0.6.2 | MIT OR Apache-2.0 OR Zlib | PASS | elected MIT |
| `read-fonts` | 0.37.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `read-fonts` | 0.39.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `regex` | 1.12.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `regex-automata` | 0.4.14 | MIT OR Apache-2.0 | PASS | elected MIT |
| `regex-syntax` | 0.8.11 | MIT OR Apache-2.0 | PASS | elected MIT |
| `rendercache` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `renderdoc-sys` | 1.1.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `reqwest` | 0.13.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `rsmpeg` | 0.18.0+ffmpeg.8.0 | MIT | PASS |  |
| `rustc-hash` | 1.1.0 | Apache-2.0/MIT | PASS | elected Apache-2.0 |
| `rustc-hash` | 2.1.2 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `rustls` | 0.23.41 | Apache-2.0 OR ISC OR MIT | PASS | elected Apache-2.0 |
| `rustls-pki-types` | 1.15.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `rustls-platform-verifier` | 0.7.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `rustls-webpki` | 0.103.13 | ISC | PASS |  |
| `rustversion` | 1.0.22 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `rusty_ffmpeg` | 0.16.7+ffmpeg.8 | MIT | PASS |  |
| `scopeguard` | 1.2.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `self_cell` | 1.2.2 | Apache-2.0 OR GPL-2.0-only | WARN | elected Apache-2.0 from Apache-2.0 OR GPL-2.0-only; not relied on: GPL-2.0-only (incompatible) |
| `serde` | 1.0.228 | MIT OR Apache-2.0 | PASS | elected MIT |
| `serde_core` | 1.0.228 | MIT OR Apache-2.0 | PASS | elected MIT |
| `serde_derive` | 1.0.228 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `serde_json` | 1.0.150 | MIT OR Apache-2.0 | PASS | elected MIT |
| `simd-adler32` | 0.3.9 | MIT | PASS |  |
| `skrifa` | 0.40.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `skrifa` | 0.42.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `slab` | 0.4.12 | MIT | PASS |  |
| `slotmap` | 1.1.1 | Zlib | PASS |  |
| `smallvec` | 1.15.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `smol_str` | 0.3.6 | MIT OR Apache-2.0 | PASS | elected MIT |
| `socket2` | 0.6.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `spirv` | 0.3.0+sdk-1.3.268.0 | Apache-2.0 | PASS |  |
| `spirv` | 0.4.0+sdk-1.4.341.0 | Apache-2.0 | PASS |  |
| `stable_deref_trait` | 1.2.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `static_assertions` | 1.1.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `strsim` | 0.11.1 | MIT | PASS |  |
| `subtle` | 2.6.1 | BSD-3-Clause | PASS |  |
| `swash` | 0.2.9 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `syn` | 2.0.118 | MIT OR Apache-2.0 | PASS | elected MIT |
| `syn` | 3.0.3 | MIT OR Apache-2.0 | PASS | elected MIT |
| `sync_wrapper` | 1.0.2 | Apache-2.0 | PASS |  |
| `synstructure` | 0.13.2 | MIT | PASS |  |
| `sys-locale` | 0.3.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `tempfile` | 3.27.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `thiserror` | 1.0.69 | MIT OR Apache-2.0 | PASS | elected MIT |
| `thiserror` | 2.0.18 | MIT OR Apache-2.0 | PASS | elected MIT |
| `thiserror-impl` | 1.0.69 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `thiserror-impl` | 2.0.18 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `timeline-render` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `tinystr` | 0.8.3 | Unicode-3.0 | PASS |  |
| `tinyvec` | 1.11.0 | Zlib OR Apache-2.0 OR MIT | PASS | elected Zlib |
| `tinyvec_macros` | 0.1.1 | MIT OR Apache-2.0 OR Zlib | PASS | elected MIT |
| `tokio` | 1.52.3 | MIT | PASS |  |
| `tokio-rustls` | 0.26.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `tokio-util` | 0.7.18 | MIT | PASS |  |
| `tower` | 0.5.3 | MIT | PASS |  |
| `tower-http` | 0.6.11 | MIT | PASS |  |
| `tower-layer` | 0.3.3 | MIT | PASS |  |
| `tower-service` | 0.3.3 | MIT | PASS |  |
| `tracing` | 0.1.44 | MIT | PASS |  |
| `tracing-core` | 0.1.36 | MIT | PASS |  |
| `try-lock` | 0.2.5 | MIT | PASS |  |
| `ttf-parser` | 0.25.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `unicase` | 2.9.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `unicode-bidi` | 0.3.18 | MIT OR Apache-2.0 | PASS | elected MIT |
| `unicode-ident` | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 | PASS | elected MIT AND Unicode-3.0 |
| `unicode-linebreak` | 0.1.5 | Apache-2.0 | PASS |  |
| `unicode-script` | 0.5.8 | MIT OR Apache-2.0 | PASS | elected MIT |
| `unicode-segmentation` | 1.13.3 | MIT OR Apache-2.0 | PASS | elected MIT |
| `unicode-width` | 0.2.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `untrusted` | 0.9.0 | ISC | PASS |  |
| `url` | 2.5.8 | MIT OR Apache-2.0 | PASS | elected MIT |
| `utf8_iter` | 1.0.4 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `want` | 0.3.1 | MIT | PASS |  |
| `waveform` | 0.1.0 | AGPL-3.0-only | PASS | first-party (workspace crate) |
| `wgpu` | 26.0.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `wgpu` | 29.0.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `wgpu-core` | 26.0.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `wgpu-core` | 29.0.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `wgpu-core-deps-windows-linux-android` | 26.0.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `wgpu-core-deps-windows-linux-android` | 29.0.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `wgpu-hal` | 26.0.6 | MIT OR Apache-2.0 | PASS | elected MIT |
| `wgpu-hal` | 29.0.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `wgpu-naga-bridge` | 29.0.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `wgpu-types` | 26.0.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `wgpu-types` | 29.0.4 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows` | 0.58.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows` | 0.62.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows_x86_64_msvc` | 0.52.6 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-collections` | 0.3.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-core` | 0.58.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-core` | 0.62.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-future` | 0.3.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-implement` | 0.58.0 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `windows-implement` | 0.60.2 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `windows-interface` | 0.58.0 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `windows-interface` | 0.59.3 | MIT OR Apache-2.0 | PASS | proc-macro (build-time); elected MIT |
| `windows-link` | 0.2.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-native-keyring-store` | 1.1.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-numerics` | 0.3.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-registry` | 0.6.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-result` | 0.2.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-result` | 0.4.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-strings` | 0.1.0 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-strings` | 0.5.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-sys` | 0.61.2 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-targets` | 0.52.6 | MIT OR Apache-2.0 | PASS | elected MIT |
| `windows-threading` | 0.2.1 | MIT OR Apache-2.0 | PASS | elected MIT |
| `writeable` | 0.6.3 | Unicode-3.0 | PASS |  |
| `yazi` | 0.2.1 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `yoke` | 0.8.3 | Unicode-3.0 | PASS |  |
| `yoke-derive` | 0.8.2 | Unicode-3.0 | PASS | proc-macro (build-time) |
| `zeno` | 0.3.3 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `zerocopy` | 0.8.52 | BSD-2-Clause OR Apache-2.0 OR MIT | PASS | elected BSD-2-Clause |
| `zerocopy-derive` | 0.8.52 | BSD-2-Clause OR Apache-2.0 OR MIT | PASS | proc-macro (build-time); elected BSD-2-Clause |
| `zerofrom` | 0.1.8 | Unicode-3.0 | PASS |  |
| `zerofrom-derive` | 0.1.7 | Unicode-3.0 | PASS | proc-macro (build-time) |
| `zeroize` | 1.9.0 | Apache-2.0 OR MIT | PASS | elected Apache-2.0 |
| `zerotrie` | 0.2.4 | Unicode-3.0 | PASS |  |
| `zerovec` | 0.11.6 | Unicode-3.0 | PASS |  |
| `zerovec-derive` | 0.11.3 | Unicode-3.0 | PASS | proc-macro (build-time) |
| `zmij` | 1.0.21 | MIT | PASS |  |
| `zune-core` | 0.5.1 | MIT OR Apache-2.0 OR Zlib | PASS | elected MIT |
| `zune-jpeg` | 0.5.15 | MIT OR Apache-2.0 OR Zlib | PASS | elected MIT |

## 3. Direct dependencies recorded in PROVENANCE.md

Every direct normal/build third-party dependency of a workspace crate must be named in `PROVENANCE.md`.

| Dependency | Result | Detail |
|---|---|---|
| `anyhow` | PASS | named in PROVENANCE.md (first mention: Entry 40); direct dep of engine |
| `base64` | PASS | named in PROVENANCE.md (first mention: Entry 8); direct dep of agent-gen, agent-llm, app-core |
| `bytemuck` | PASS | named in PROVENANCE.md (first mention: Entry 26); direct dep of engine, timeline-render |
| `cosmic-text` | PASS | named in PROVENANCE.md (first mention: Entry 10); direct dep of engine |
| `cpal` | PASS | named in PROVENANCE.md (first mention: Entry 4); direct dep of engine |
| `etagere` | PASS | named in PROVENANCE.md (first mention: Entry 26); direct dep of timeline-render |
| `glyphon` | PASS | named in PROVENANCE.md (first mention: Entry 26); direct dep of timeline-render |
| `image` | PASS | named in PROVENANCE.md (first mention: Entry 2); direct dep of app-core, engine, filmstrip |
| `keyring` | PASS | named in PROVENANCE.md (first mention: Entry 9); direct dep of agent-llm |
| `linkme` | PASS | named in PROVENANCE.md (first mention: Entry 20); direct dep of ffi |
| `pollster` | PASS | named in PROVENANCE.md (first mention: Entry 26); direct dep of engine, timeline-render |
| `reqwest` | PASS | named in PROVENANCE.md (first mention: Entry 7); direct dep of agent-gen, agent-llm |
| `rmcp` | PASS | named in PROVENANCE.md (first mention: Entry 6); direct dep of agent-mcp |
| `rsmpeg` | PASS | named in PROVENANCE.md (first mention: Entry 5); direct dep of engine |
| `schemars` | PASS | named in PROVENANCE.md (first mention: Entry 6); direct dep of agent-mcp |
| `serde` | PASS | named in PROVENANCE.md (first mention: Entry 6); direct dep of agent-gen, agent-llm, agent-mcp, agent-tools, app-core, core, engine, ffi, filmstrip, preview, proxy, rendercache, waveform |
| `serde_json` | PASS | named in PROVENANCE.md (first mention: Entry 6); direct dep of agent-gen, agent-llm, agent-mcp, agent-tools, app-core, engine, ffi, filmstrip, proxy, rendercache, waveform |
| `tempfile` | PASS | named in PROVENANCE.md (first mention: Entry 40); direct dep of ffi |
| `thiserror` | PASS | named in PROVENANCE.md (first mention: Entry 6); direct dep of agent-gen, agent-llm, app-core, core, engine, filmstrip, waveform |
| `tokio` | PASS | named in PROVENANCE.md (first mention: Entry 6); direct dep of agent-mcp, app-core, ffi |
| `wgpu` | PASS | named in PROVENANCE.md (first mention: Entry 26); direct dep of engine, ffi, timeline-render |
| `wgpu-hal` | PASS | named in PROVENANCE.md (first mention: Entry 21); direct dep of ffi, timeline-render |
| `windows` | PASS | named in PROVENANCE.md (first mention: header); direct dep of composition-scale, engine, ffi, timeline-render |
| `windows-core` | PASS | named in PROVENANCE.md (first mention: Entry 28); direct dep of ffi |
| `dotenvy` | INFO | dev-dependency only (agent-llm, ffi); not shipped |
| `libloading` | INFO | dev-dependency only (ffi); not shipped |
| `object` | INFO | dev-dependency only (ffi); not shipped |
| `tao` | INFO | dev-dependency only (engine); not shipped |
| `windows-sys` | INFO | dev-dependency only (composition-scale, ffi); not shipped |

## 4. NuGet packages (61 rows)

Read from each `packages.lock.json` and the package's `.nuspec`. `SHIPS` = in the shipped `Rudis.Shell` graph.

| Project: package | Version | Licence | Ships | Result | PROVENANCE | Disposition / note |
|---|---|---|---|---|---|---|
| `Rudis.Shell: Microsoft.Web.WebView2` | 1.0.3179.45 | BSD-3-Clause | SHIPS | PASS | Entry 23 | license type=file -> disposition: nuspec license type=file; LICENSE.txt is the BSD-3-Clause text (Microsoft copyright).  |
| `Rudis.Shell: Microsoft.Windows.SDK.BuildTools` | 10.0.26100.8249 | Microsoft Windows SDK licence terms (build tool) | BUILD-ONLY | PASS | Entry 23 | build-only, not redistributed: build-time tool (winmd projections / MSIX tooling); nothing from it is redistributed in the app payload, so no licence combination arises. |
| `Rudis.Shell: Microsoft.Windows.SDK.BuildTools.MSIX` | 1.7.20250829.1 | Microsoft Windows SDK licence terms (build tool) | BUILD-ONLY | PASS | Entry 23 | build-only, not redistributed: build-time tool (winmd projections / MSIX tooling); nothing from it is redistributed in the app payload, so no licence combination arises. |
| `Rudis.Shell: Microsoft.WindowsAppSDK` | 1.8.260710003 | Microsoft Software License Terms (proprietary, redistributable) | SHIPS | WARN | Entry 23, Entry 40 | ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it. |
| `Rudis.Shell: Microsoft.WindowsAppSDK.AI` | 1.8.79 | Microsoft Software License Terms (proprietary, redistributable) | SHIPS | WARN | Entry 23, Entry 40 | ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it. |
| `Rudis.Shell: Microsoft.WindowsAppSDK.Base` | 1.8.251216001 | Microsoft Software License Terms (proprietary, redistributable) | SHIPS | WARN | Entry 23, Entry 40 | ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it. |
| `Rudis.Shell: Microsoft.WindowsAppSDK.DWrite` | 1.8.25122902 | Microsoft Software License Terms (proprietary, redistributable) | SHIPS | WARN | Entry 23, Entry 40 | ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it. |
| `Rudis.Shell: Microsoft.WindowsAppSDK.Foundation` | 1.8.260709000 | Microsoft Software License Terms (proprietary, redistributable) | SHIPS | WARN | Entry 23, Entry 40 | ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it. |
| `Rudis.Shell: Microsoft.WindowsAppSDK.InteractiveExperiences` | 1.8.260708001 | Microsoft Software License Terms (proprietary, redistributable) | SHIPS | WARN | Entry 23, Entry 40 | ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it. |
| `Rudis.Shell: Microsoft.WindowsAppSDK.ML` | 1.8.2197 | Microsoft Software License Terms (proprietary, redistributable) | SHIPS | WARN | Entry 23, Entry 40 | ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it. |
| `Rudis.Shell: Microsoft.WindowsAppSDK.Runtime` | 1.8.260710003 | Microsoft Software License Terms (proprietary, redistributable) | SHIPS | WARN | Entry 23, Entry 40 | ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it. |
| `Rudis.Shell: Microsoft.WindowsAppSDK.Widgets` | 1.8.251231004 | Microsoft Software License Terms (proprietary, redistributable) | SHIPS | WARN | Entry 23, Entry 40 | ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it. |
| `Rudis.Shell: Microsoft.WindowsAppSDK.WinUI` | 1.8.260709004 | Microsoft Software License Terms (proprietary, redistributable) | SHIPS | WARN | Entry 23, Entry 40 | ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it. |
| `Rudis.Shell: System.Numerics.Tensors` | 9.0.0 | MIT | SHIPS | PASS |  |  |
| `Rudis.Shell: Velopack` | 1.2.0 | MIT | SHIPS | PASS |  |  |
| `Rudis.Shell.EvalHarness` |  |  |  | INFO |  | no packages.lock.json (dev/test harness, not shipped); not reconciled |
| `Rudis.Shell.Tests: Microsoft.CodeCoverage` | 17.12.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: Microsoft.NET.Test.Sdk` | 17.12.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: Microsoft.TestPlatform.ObjectModel` | 17.12.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: Microsoft.TestPlatform.TestHost` | 17.12.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: Newtonsoft.Json` | 13.0.1 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: System.Reflection.Metadata` | 1.6.0 | MIT | TEST-ONLY | PASS | Entry 23 | licenseUrl only -> disposition: nuspec licenseUrl only (dotnet/corefx LICENSE.TXT = MIT). Transitive of the test SDK, test-only.  |
| `Rudis.Shell.Tests: xunit` | 2.9.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: xunit.abstractions` | 2.0.3 | Apache-2.0 | TEST-ONLY | PASS | Entry 23 | licenseUrl only -> disposition: nuspec licenseUrl only (xunit license.txt = Apache-2.0). Transitive of xunit, test-only.  |
| `Rudis.Shell.Tests: xunit.analyzers` | 1.16.0 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: xunit.assert` | 2.9.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: xunit.core` | 2.9.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: xunit.extensibility.core` | 2.9.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: xunit.extensibility.execution` | 2.9.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.Tests: xunit.runner.visualstudio` | 2.8.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: FlaUI.Core` | 5.0.0 | MIT | TEST-ONLY | PASS | Entry 24 | license type=file -> disposition: nuspec license type=file; LICENSE.txt is the MIT text. UIA test harness only.  |
| `Rudis.Shell.UiTests: FlaUI.UIA3` | 5.0.0 | MIT | TEST-ONLY | PASS | Entry 24 | license type=file -> disposition: nuspec license type=file; LICENSE.txt is the MIT text. UIA test harness only.  |
| `Rudis.Shell.UiTests: Interop.UIAutomationClient` | 10.19041.0 | MIT | TEST-ONLY | PASS | Entry 24 | license type=file -> disposition: nuspec license type=file; LICENSE.txt is the MIT text. Transitive of FlaUI.UIA3, test-only.  |
| `Rudis.Shell.UiTests: Microsoft.CodeCoverage` | 17.12.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: Microsoft.NET.Test.Sdk` | 17.12.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: Microsoft.NETCore.Platforms` | 5.0.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: Microsoft.TestPlatform.ObjectModel` | 17.12.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: Microsoft.TestPlatform.TestHost` | 17.12.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: Microsoft.Win32.Registry` | 5.0.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: Microsoft.Win32.SystemEvents` | 8.0.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: Newtonsoft.Json` | 13.0.1 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.CodeDom` | 8.0.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.Configuration.ConfigurationManager` | 8.0.1 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.Diagnostics.EventLog` | 8.0.1 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.Diagnostics.PerformanceCounter` | 8.0.1 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.Drawing.Common` | 8.0.10 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.Management` | 8.0.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.Reflection.Metadata` | 1.6.0 | MIT | TEST-ONLY | PASS | Entry 23 | licenseUrl only -> disposition: nuspec licenseUrl only (dotnet/corefx LICENSE.TXT = MIT). Transitive of the test SDK, test-only.  |
| `Rudis.Shell.UiTests: System.Security.AccessControl` | 5.0.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.Security.Cryptography.ProtectedData` | 8.0.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.Security.Permissions` | 8.0.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.Security.Principal.Windows` | 5.0.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: System.Windows.Extensions` | 8.0.0 | MIT | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: xunit` | 2.9.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: xunit.abstractions` | 2.0.3 | Apache-2.0 | TEST-ONLY | PASS | Entry 23 | licenseUrl only -> disposition: nuspec licenseUrl only (xunit license.txt = Apache-2.0). Transitive of xunit, test-only.  |
| `Rudis.Shell.UiTests: xunit.analyzers` | 1.16.0 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: xunit.assert` | 2.9.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: xunit.core` | 2.9.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: xunit.extensibility.core` | 2.9.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: xunit.extensibility.execution` | 2.9.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |
| `Rudis.Shell.UiTests: xunit.runner.visualstudio` | 2.8.2 | Apache-2.0 | TEST-ONLY | PASS |  |  |

## 5. Bundled binaries, assets and platform components

`runtime/binaries/` and `crates/engine/ffmpeg-dev/` are fetched by the scripts under `scripts/windows/`
(not committed); the fonts are tracked. The tracker sidecar under `runtime/binaries/opencv/` is one component.

| Component | Licence | Ships | Result | PROVENANCE | Note |
|---|---|---|---|---|---|
| `runtime/binaries/avcodec-63.dll` | LGPL-3.0-or-later | SHIPS | PASS | Entry 5 |  |
| `runtime/binaries/avdevice-63.dll` | LGPL-3.0-or-later | SHIPS | PASS | Entry 5 |  |
| `runtime/binaries/avfilter-12.dll` | LGPL-3.0-or-later | SHIPS | PASS | Entry 5 |  |
| `runtime/binaries/avformat-63.dll` | LGPL-3.0-or-later | SHIPS | PASS | Entry 5 |  |
| `runtime/binaries/avutil-61.dll` | LGPL-3.0-or-later | SHIPS | PASS | Entry 5 |  |
| `runtime/binaries/ffmpeg.exe` | LGPL-3.0-or-later | SHIPS | PASS | Entry 5 |  |
| `runtime/binaries/ffprobe.exe` | LGPL-3.0-or-later | SHIPS | PASS | Entry 5 |  |
| `runtime/binaries/ggml.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-base.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-cpu-alderlake.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-cpu-cannonlake.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-cpu-cascadelake.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-cpu-haswell.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-cpu-icelake.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-cpu-sandybridge.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-cpu-skylakex.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-cpu-sse42.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-cpu-x64.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/ggml-small.bin` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/opencv/**` | PSF-2.0 AND Apache-2.0 AND MIT AND BSD-3-Clause AND 0BSD AND Zlib AND CC0-1.0 AND MPL-2.0 AND LGPL-2.1-or-later AND GPL-3.0-or-later WITH GCC-exception-3.1 AND AGPL-3.0-only | SHIPS | WARN | Entry 12, Entry 40 | 2404 files; proprietary-flagged: bundles Microsoft Visual C++ runtime DLLs (vcruntime140.dll, vcruntime140_1.dll from the python.org embeddable; msvcp140-*.dll in numpy.libs) under the Visual Studio redistributable terms - ACCEPTED by the owner 2026-09-30 as an AGPL-3.0 s1 System Library: a compiler runtime is the textbook case (the Major Component / compiler clause), same decision as the Windows App SDK. |
| `runtime/binaries/parakeet.dll` | MIT | SHIPS | PASS | Entry 40 |  |
| `runtime/binaries/SDL2.dll` | Zlib | SHIPS | PASS | Entry 40 |  |
| `runtime/binaries/swresample-7.dll` | LGPL-3.0-or-later | SHIPS | PASS | Entry 5 |  |
| `runtime/binaries/swscale-10.dll` | LGPL-3.0-or-later | SHIPS | PASS | Entry 5 |  |
| `runtime/binaries/whisper.dll` | MIT | SHIPS | PASS | Entry 11 |  |
| `runtime/binaries/whisper-cli.exe` | MIT | SHIPS | PASS | Entry 11 |  |
| `crates/engine/ffmpeg-dev/bin/avcodec-62.dll` | LGPL-3.0-or-later | SHIPS | WARN | Entry 21 | patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence. |
| `crates/engine/ffmpeg-dev/bin/avdevice-62.dll` | LGPL-3.0-or-later | SHIPS | WARN | Entry 21 | patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence. |
| `crates/engine/ffmpeg-dev/bin/avfilter-11.dll` | LGPL-3.0-or-later | SHIPS | WARN | Entry 21 | patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence. |
| `crates/engine/ffmpeg-dev/bin/avformat-62.dll` | LGPL-3.0-or-later | SHIPS | WARN | Entry 21 | patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence. |
| `crates/engine/ffmpeg-dev/bin/avutil-60.dll` | LGPL-3.0-or-later | SHIPS | WARN | Entry 21 | patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence. |
| `crates/engine/ffmpeg-dev/bin/ffmpeg.exe` | LGPL-3.0-or-later | NOT SHIPPED | PASS | Entry 21 |  |
| `crates/engine/ffmpeg-dev/bin/ffplay.exe` | LGPL-3.0-or-later | NOT SHIPPED | PASS | Entry 21 |  |
| `crates/engine/ffmpeg-dev/bin/ffprobe.exe` | LGPL-3.0-or-later | NOT SHIPPED | PASS | Entry 21 |  |
| `crates/engine/ffmpeg-dev/bin/swresample-6.dll` | LGPL-3.0-or-later | SHIPS | WARN | Entry 21 | patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence. |
| `crates/engine/ffmpeg-dev/bin/swscale-9.dll` | LGPL-3.0-or-later | SHIPS | WARN | Entry 21 | patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence. |
| `crates/engine/assets/fonts/Inter-Bold.ttf` | OFL-1.1 | SHIPS | PASS | Entry 10 |  |
| `crates/engine/assets/fonts/Inter-BoldItalic.ttf` | OFL-1.1 | SHIPS | PASS | Entry 10 |  |
| `crates/engine/assets/fonts/Inter-Italic.ttf` | OFL-1.1 | SHIPS | PASS | Entry 10 |  |
| `crates/engine/assets/fonts/Inter-Regular.ttf` | OFL-1.1 | SHIPS | PASS | Entry 10 |  |
| `crates/engine/assets/fonts/OFL.txt` | OFL-1.1 | SHIPS | PASS | Entry 10 |  |
| `.NET 9 runtime (self-contained publish)` | MIT | SHIPS | PASS | Entry 40 | platform component (not a NuGet row) |

## 6. FFmpeg payload claims (NOTICE.md), measured

Each `payload-claim` marker in `NOTICE.md` is re-measured on the real file: `ffmpeg -encoders` for an
`.exe` (plus `-L` = LGPL and `-buildconf` free of gpl/nonfree/x264/x265), a string scan for a `.dll` that
ignores `--disable-`/`--enable-` configure tokens.

| Payload | Encoders | NOTICE claims | Measured | Found | Version | Result |
|---|---|---|---|---|---|---|
| `runtime/binaries/ffmpeg.exe` | libopenh264, libkvazaar, libvvenc | ABSENT | ABSENT | 0 of 3 | N-125907-ga7e72069f1-20260830 | PASS |
| `runtime/binaries/avcodec-63.dll` | libopenh264, libkvazaar, libvvenc | ABSENT | ABSENT | 0 of 3 | file version 63.7.100 | PASS |
| `crates/engine/ffmpeg-dev/bin/ffmpeg.exe` | libopenh264, libkvazaar, libvvenc | PRESENT | PRESENT | 3 of 3 | n8.0.1-48-g0592be14ff-20260131 | PASS |
| `crates/engine/ffmpeg-dev/bin/avcodec-62.dll` | libopenh264, libkvazaar, libvvenc | PRESENT | PRESENT | 3 of 3 | file version 62.11.100 | PASS |

## 7. Flags

Every WARN row above, with its written disposition. None of these is a clearance.

- **[2] `self_cell 1.2.2`** -- elected Apache-2.0 from Apache-2.0 OR GPL-2.0-only; not relied on: GPL-2.0-only (incompatible)
- **[4] `Rudis.Shell: Microsoft.WindowsAppSDK 1.8.260710003`** -- ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it.
- **[4] `Rudis.Shell: Microsoft.WindowsAppSDK.AI 1.8.79`** -- ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it.
- **[4] `Rudis.Shell: Microsoft.WindowsAppSDK.Base 1.8.251216001`** -- ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it.
- **[4] `Rudis.Shell: Microsoft.WindowsAppSDK.DWrite 1.8.25122902`** -- ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it.
- **[4] `Rudis.Shell: Microsoft.WindowsAppSDK.Foundation 1.8.260709000`** -- ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it.
- **[4] `Rudis.Shell: Microsoft.WindowsAppSDK.InteractiveExperiences 1.8.260708001`** -- ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it.
- **[4] `Rudis.Shell: Microsoft.WindowsAppSDK.ML 1.8.2197`** -- ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it.
- **[4] `Rudis.Shell: Microsoft.WindowsAppSDK.Runtime 1.8.260710003`** -- ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it.
- **[4] `Rudis.Shell: Microsoft.WindowsAppSDK.Widgets 1.8.251231004`** -- ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it.
- **[4] `Rudis.Shell: Microsoft.WindowsAppSDK.WinUI 1.8.260709004`** -- ACCEPTED by the owner 2026-09-30 under the usual reading: a platform UI runtime distributed out-of-band by the OS vendor falls under AGPL-3.0 s1's System Libraries carve-out (the posture every (A)GPL WPF/WinUI/.NET desktop app takes). Proprietary, so it stays listed on every run; no s7 additional permission was added. Revisit only if a contributor or distributor raises it.
- **[5] `runtime/binaries/opencv/**`** -- 2404 files; proprietary-flagged: bundles Microsoft Visual C++ runtime DLLs (vcruntime140.dll, vcruntime140_1.dll from the python.org embeddable; msvcp140-*.dll in numpy.libs) under the Visual Studio redistributable terms - ACCEPTED by the owner 2026-09-30 as an AGPL-3.0 s1 System Library: a compiler runtime is the textbook case (the Major Component / compiler clause), same decision as the Windows App SDK.
- **[5] `crates/engine/ffmpeg-dev/bin/avcodec-62.dll`** -- patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence.
- **[5] `crates/engine/ffmpeg-dev/bin/avdevice-62.dll`** -- patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence.
- **[5] `crates/engine/ffmpeg-dev/bin/avfilter-11.dll`** -- patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence.
- **[5] `crates/engine/ffmpeg-dev/bin/avformat-62.dll`** -- patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence.
- **[5] `crates/engine/ffmpeg-dev/bin/avutil-60.dll`** -- patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence.
- **[5] `crates/engine/ffmpeg-dev/bin/swresample-6.dll`** -- patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence.
- **[5] `crates/engine/ffmpeg-dev/bin/swscale-9.dll`** -- patent: libopenh264/libkvazaar/libvvenc present in this payload's avcodec-62.dll (D-62-01-01); Rudis never selects them, but copyright licensing and patent licensing are different axes and the AGPL grants no codec patent licence.

