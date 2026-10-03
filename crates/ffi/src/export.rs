//! D-08's structural panic-guard enforcement: the [`FFI_EXPORTS`] registry and
//! the [`ffi_guard!`] macro, whose expansion makes registration and
//! `catch_unwind` inseparable.
//!
//! # DESIGN NOTE (D-08's one point of shaped discretion — recorded per plan 47-02)
//!
//! The export ITEM (`#[no_mangle] pub extern "C" fn ...`) is written literally
//! at module level rather than emitted by the macro, because `cbindgen` parses
//! source with `syn` and CANNOT see macro_rules-generated items on the pinned
//! stable toolchain (`parse.expand` requires nightly). CONTEXT lists "the exact
//! macro name and expansion shape for D-08" as Claude's Discretion; D-08's
//! three load-bearing properties all survive this shape:
//!
//! (a) enforcement is structural, not a source grep;
//! (b) ground truth is the built DLL's export table (47-07 compares it to
//!     [`FFI_EXPORTS`], set-equal both directions);
//! (c) the `catch_unwind` wrapper and the registration are inseparable — you
//!     cannot register a symbol without also getting the guard, and an
//!     unregistered export turns the table test red.
//!
//! Plain `extern "C"`, never the unwind-capable ABI string (RESEARCH B4:
//! post-1.71 a bypassed panic at a plain `"C"` boundary is a defined abort,
//! the safe last resort). RESEARCH B3 audited every profile: nothing sets
//! `panic = "abort"`, so `catch_unwind` is live everywhere — re-confirmed on
//! this tree 2026-07-29 (zero hits for the abort setting over every manifest,
//! zero `[profile.*]` sections anywhere). If a profile ever sets abort, STOP:
//! FFI-02 is unsatisfiable and the phase must halt rather than ship a no-op
//! guard.

/// D-08's registry: every ABI export registers itself here via `ffi_guard!`.
/// 47-07's export_table test asserts the built DLL's export table is
/// EXACTLY this set — a hand-written `#[no_mangle]` that skips `ffi_guard!`
/// shows up in the DLL but not here, and fails the build gates.
#[linkme::distributed_slice]
pub static FFI_EXPORTS: [&'static str] = [..];

/// Panic-payload to text: downcast `&str`, then `String`, else a placeholder.
/// Reimplements the Phase-44 spike's downcast shape (technique only — the
/// spike is workspace-excluded and its code never ships).
#[doc(hidden)]
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

/// The ONLY legal way to give an `extern "C"` export a body (D-08).
/// Expands to: (a) a fn-local linkme registration static tying the symbol
/// name into [`FFI_EXPORTS`], and (b) `catch_unwind(AssertUnwindSafe(body))`
/// with a caught panic mapped to `$panic_ret`. Registration and panic-guard
/// come from the SAME expansion and cannot be had separately.
///
/// A caught panic is logged to stderr (the only observation made after a
/// panic — nothing else is read, which is what makes `AssertUnwindSafe`
/// sound here). The store mutex is poison-SAFE, not poison-recoverable
/// (IN-01): a panic that unwinds while the store lock is held poisons it,
/// and every `app_core` `run_*` body does `store().lock().map_err(..)?` —
/// `std::sync::Mutex` never un-poisons (no `clear_poison()` anywhere in this
/// tree), so every later store-touching export on that ctx returns the
/// `{"Err": "backend store mutex poisoned"}` envelope for the rest of the
/// ctx's life. Graceful degradation (no crash, no UB) — never self-healing —
/// until `rudis_shutdown` + `rudis_init` build a fresh ctx. Inherited
/// verbatim from the Tauri host (`src-tauri/src/lib.rs` uses the identical
/// string); a Phase-50 host that sees this error persist should recycle its
/// `RudisCtx` rather than wait for the store to clear itself.
#[macro_export]
macro_rules! ffi_guard {
    ($name:literal, $panic_ret:expr, $body:block) => {{
        #[linkme::distributed_slice($crate::export::FFI_EXPORTS)]
        static REG: &'static str = $name;
        match ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| $body)) {
            Ok(v) => v,
            Err(payload) => {
                ::std::eprintln!(
                    "[rudis_ffi] panic caught at the `{}` boundary (letting it unwind \
                     across the C ABI would be UB): {}",
                    $name,
                    $crate::export::panic_message(payload.as_ref())
                );
                $panic_ret
            }
        }
    }};
}
