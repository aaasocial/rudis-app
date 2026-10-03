use std::path::PathBuf;

fn main() {
    // NOTE: because this script emits `cargo:rerun-if-changed` directives below,
    // cargo no longer reruns it on every source change — only when build.rs
    // itself or the watched toolchain-pin dirs change. That is the intent.
    println!("cargo:rerun-if-changed=build.rs");

    // Windows-only: `examples/surface_spike.rs` (Phase 9 Wave 1) links `tao`,
    // which imports comctl32 v6-only exports (SetWindowSubclass / DefSubclassProc
    // / RemoveWindowSubclass / TaskDialogIndirect). cargo gives example harnesses
    // no application manifest, so without the Common-Controls v6 dependency the
    // loader binds comctl32 v5.82, the exports are missing, and the process dies
    // at startup with STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139) before main runs.
    // Embed a v6 manifest into EXAMPLE targets (this crate ships no bin, so there
    // is nothing to collide with). Mirrored the same fix the then-live
    // src-tauri/build.rs carried; that shell was retired at Phase 55 (GATE-07),
    // and the native C# shell gets its v6 manifest from the .NET SDK instead.
    // Only when the examples exist: cargo rejects `rustc-link-arg-examples` for a package
    // with no example targets, and the public source tree ships without `examples/`.
    let has_examples = std::path::Path::new(
        &std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"),
    )
    .join("examples")
    .is_dir();
    if has_examples && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security></trustInfo>
  <dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
</assembly>
"#;
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
        let manifest_path = std::path::Path::new(&out_dir).join("example_comctl32.manifest");
        std::fs::write(&manifest_path, MANIFEST).expect("failed to write example manifest");
        println!("cargo:rustc-link-arg-examples=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-examples=/MANIFESTINPUT:{}",
            manifest_path.display()
        );
    }

    // Phase 48 (plan 48-03): toolchain-pin guards + runtime DLL staging for the
    // `hwdecode` feature (in-process rsmpeg D3D11VA preview decode).
    //
    // Both guards exist because the failure they catch is otherwise SILENT or
    // deferred: a wrong libclang makes bindgen 0.71.1 emit incomplete types
    // WITHOUT erroring (Phase 44's measured hazard), and missing/mismatched
    // FFmpeg headers surface only as confusing rsmpeg compile errors. Fail HERE,
    // LOUDLY, with the fetch script named.
    if std::env::var("CARGO_FEATURE_HWDECODE").is_ok()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
    {
        guard_libclang_pin();
        guard_ffmpeg_devlibs_pin();
        stage_ffmpeg_runtime_dlls();
    }
}

/// Pin 1 guard — libclang 18.1.1 (48-CONTEXT.md D-04).
///
/// The hazard is SILENT: bindgen 0.71.1 + libclang 22 emits silently incomplete
/// types (AVFormatContext -> { _address: u8 }) while still passing its own
/// layout assertion — so "bindgen succeeded" proves nothing. The only reliable
/// check is IDENTITY: the dll bindgen will load must be byte-identical to the
/// one the fetch script pinned (SHA256 sidecar written at fetch time).
fn guard_libclang_pin() {
    const FIX: &str = "run scripts/windows/fetch-libclang18.ps1 (and scripts/windows/fetch-ffmpeg-devlibs.ps1 for the FFmpeg pin)";
    const HAZARD: &str = "bindgen 0.71.1 + libclang 22 emits silently incomplete types (AVFormatContext -> { _address: u8 }) while still passing its own layout assertion — run scripts/windows/fetch-libclang18.ps1";

    let libclang_path = std::env::var("LIBCLANG_PATH").unwrap_or_else(|_| {
        panic!(
            "hwdecode: LIBCLANG_PATH is not set. The root .cargo/config.toml [env] table \
             should provide it; {FIX}. Why this matters: {HAZARD}"
        )
    });
    let dir = PathBuf::from(&libclang_path);
    println!("cargo:rerun-if-changed={}", dir.display());

    let dll = dir.join("libclang.dll");
    if !dll.is_file() {
        panic!(
            "hwdecode: LIBCLANG_PATH ({}) contains no libclang.dll — {FIX}. \
             Why this matters: {HAZARD}",
            dir.display()
        );
    }
    let sidecar = dir.join("libclang.dll.sha256");
    let expected = std::fs::read_to_string(&sidecar).unwrap_or_else(|e| {
        panic!(
            "hwdecode: cannot read the pin sidecar {} ({e}). This libclang.dll is NOT \
             the hash-pinned 18.1.1 payload — {FIX}. Why this matters: {HAZARD}",
            sidecar.display()
        )
    });
    let expected = expected.trim().to_ascii_lowercase();

    let bytes = std::fs::read(&dll)
        .unwrap_or_else(|e| panic!("hwdecode: cannot read {} ({e})", dll.display()));
    let actual = sha256_hex(&bytes);
    if actual != expected {
        panic!(
            "hwdecode: libclang.dll at {} does NOT match its pinned SHA256.\n  expected {expected}\n  actual   {actual}\n\
             This is the WRONG libclang for bindgen 0.71.1 — {FIX}. Why this matters: {HAZARD}",
            dll.display()
        );
    }
}

/// Pin 2 guard — the version-matched LGPL FFmpeg shared DEV build.
///
/// rsmpeg 0.18 targets avcodec-62 (FFmpeg 8.0.x); the vendored production
/// sidecar (`runtime/binaries/`, avcodec-63 git snapshot) is unlinkable at any
/// version (no headers, no import libs). Assert the headers bindgen will read
/// really are the pinned major, and that the D3D11VA hwcontext header exists.
fn guard_ffmpeg_devlibs_pin() {
    const FIX: &str = "run scripts/windows/fetch-ffmpeg-devlibs.ps1";

    let include_dir = std::env::var("FFMPEG_INCLUDE_DIR").unwrap_or_else(|_| {
        panic!(
            "hwdecode: FFMPEG_INCLUDE_DIR is not set. The root .cargo/config.toml [env] \
             table should provide it; {FIX}."
        )
    });
    let include = PathBuf::from(&include_dir);
    println!("cargo:rerun-if-changed={}", include.display());

    let version_major = include.join("libavcodec").join("version_major.h");
    let text = std::fs::read_to_string(&version_major).unwrap_or_else(|e| {
        panic!(
            "hwdecode: cannot read {} ({e}) — the FFmpeg dev headers are missing; {FIX}.",
            version_major.display()
        )
    });
    let has_62 = text.lines().any(|l| {
        let l = l.split_whitespace().collect::<Vec<_>>();
        l.len() >= 3 && l[0] == "#define" && l[1] == "LIBAVCODEC_VERSION_MAJOR" && l[2] == "62"
    });
    if !has_62 {
        panic!(
            "hwdecode: {} does not define LIBAVCODEC_VERSION_MAJOR 62 — these are NOT the \
             pinned n8.0.1 (avcodec-62) headers rsmpeg 0.18 targets; {FIX}.",
            version_major.display()
        );
    }
    let hwctx = include.join("libavutil").join("hwcontext_d3d11va.h");
    if !hwctx.is_file() {
        panic!(
            "hwdecode: {} is missing — not a full FFmpeg dev include tree (the D3D11VA \
             hwcontext header is the whole point of this feature); {FIX}.",
            hwctx.display()
        );
    }
}

/// Stage the FFmpeg runtime DLLs (the 7 av*/sw* DLLs in ffmpeg-dev/bin/) into
/// the workspace target/<profile>/ and target/<profile>/deps/ directories.
///
/// Windows resolves linked DLLs from the executable's own directory, so this
/// makes every workspace bin/test binary load avcodec-62.dll et al. with ZERO
/// per-session PATH setup (`cargo test --workspace` just works). Copies are
/// skipped when the destination already has the same file length (the pins are
/// exact versions, so length is a sufficient discriminator).
fn stage_ffmpeg_runtime_dlls() {
    // FFMPEG_DLL_PATH points at ffmpeg-dev/lib (import libs, the LICENSING door
    // — rusty_ffmpeg's dynamic_linking() branch hardcodes FFmpegLinkMode::Dynamic).
    // The runtime DLLs live in the sibling bin/.
    let lib_dir = std::env::var("FFMPEG_DLL_PATH")
        .expect("hwdecode: FFMPEG_DLL_PATH not set (root .cargo/config.toml [env])");
    let bin_dir = PathBuf::from(&lib_dir)
        .parent()
        .expect("FFMPEG_DLL_PATH has no parent")
        .join("bin");
    println!("cargo:rerun-if-changed={}", bin_dir.display());
    if !bin_dir.is_dir() {
        // The include/lib guards above already failed loudly if the payload is
        // absent; reaching here without bin/ would be a half-extracted payload.
        panic!(
            "hwdecode: {} is missing — half-extracted ffmpeg-dev payload? \
             run scripts/windows/fetch-ffmpeg-devlibs.ps1",
            bin_dir.display()
        );
    }

    // OUT_DIR = <target>/<profile>/build/<pkg>-<hash>/out — the profile dir is
    // three levels up.
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR shallower than expected")
        .to_path_buf();
    let deps_dir = profile_dir.join("deps");
    // Examples run from <profile>/examples/ (the live-object evidence capture,
    // examples/hwdecode_live_objects.rs, plan 48-05) — stage there too so
    // `cargo run -p engine --example ...` needs the same zero per-session PATH
    // setup as tests. Created eagerly: on a cold build this script runs before
    // cargo has made the directory.
    let examples_dir = profile_dir.join("examples");
    let _ = std::fs::create_dir_all(&examples_dir);

    let entries = std::fs::read_dir(&bin_dir)
        .unwrap_or_else(|e| panic!("hwdecode: cannot list {} ({e})", bin_dir.display()));
    for entry in entries.flatten() {
        let src = entry.path();
        if src.extension().and_then(|e| e.to_str()) != Some("dll") {
            continue;
        }
        for dest_dir in [&profile_dir, &deps_dir, &examples_dir] {
            if !dest_dir.is_dir() {
                continue;
            }
            let dest = dest_dir.join(src.file_name().expect("dll has a file name"));
            let same_len = match (src.metadata(), dest.metadata()) {
                (Ok(s), Ok(d)) => s.len() == d.len(),
                _ => false,
            };
            if !same_len {
                std::fs::copy(&src, &dest).unwrap_or_else(|e| {
                    panic!(
                        "hwdecode: failed to stage {} -> {} ({e})",
                        src.display(),
                        dest.display()
                    )
                });
            }
        }
    }
}

/// Minimal SHA-256 (FIPS 180-4) for the libclang identity check. Implemented
/// here so the guard adds ZERO build-dependencies; the algorithm is a public
/// specification. Output format matches PowerShell Get-FileHash (lowercase hex),
/// which is what the fetch script writes into the .sha256 sidecar.
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut msg = bytes.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    let mut hex = String::with_capacity(64);
    for word in h {
        use std::fmt::Write as _;
        let _ = write!(hex, "{word:08x}");
    }
    hex
}
