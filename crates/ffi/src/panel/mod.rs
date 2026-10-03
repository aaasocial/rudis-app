//! `panel` — everything the C# shell's engine instance needs to own a preview
//! surface of its own (Phase 51, SHELL-04).
//!
//! # Why this lives here and not next to the engine
//!
//! The engine axis is FROZEN at `engine-axis-freeze` (`8bf626f2`):
//! `crates/engine/**`, `crates/preview/**`, `crates/core/**`,
//! `src-tauri/src/native_surface.rs` and `src-tauri/src/preview_host.rs` may be
//! CALLED by shell-axis phases (50-55) and never MODIFIED
//! (`49-FREEZE.md` § "The scope rule"). `crates/ffi/**` is explicitly outside
//! that set (§ "Note on what is NOT frozen"), so this is the one legitimate
//! home for a SECOND implementation of the Phase-46 ports.
//!
//! D-03 states the shape plainly: **Phase 51 implements the ports, it does not
//! modify them.** `preview::PreviewHost` and `preview::PresentSink` exist
//! precisely so a new host can arrive without an engine diff — this module is
//! that new host, and the diff against every frozen path stays empty.
//!
//! # Module map
//!
//! * [`overlay`] — the committed-ink mirror: the fade curve, the `Patch` state
//!   machine and the annotation draw dispatch. Pure, injected-clock, no GPU.
//! * [`state`] — `PreviewSurfaceState`: the GPU set, the resize atomics and the
//!   content-rect atomics. Holds GPU objects; creates none.
//! * [`sink`] — `ShellPresentSink`, the second `preview::PresentSink` impl.
//! * [`host`] — `ShellPreviewHost`, the second `preview::PreviewHost` impl.
//! * [`surface`] — plan 51-03: the `wgpu::Instance`, the
//!   `ISwapChainPanelNative` `QueryInterface` and the whole GPU-set
//!   construction. **The ONLY file in this crate that touches COM**, which is
//!   what keeps that risk reviewable in one place.
//! * [`exports`] — plan 51-03: the four `extern "C"` panel exports and the
//!   D-07 thread-affinity guard. The ONLY ABI surface in this module tree.
//!
//! # Nothing here is `pub`
//!
//! `mod panel` is private, but `cbindgen` parses this crate's SOURCE with
//! `syn` and does NOT honour module privacy — a `pub const` in here is emitted
//! into the committed C header as a `#define`, polluting the consumer's macro
//! namespace (plan 51-03 found and fixed exactly that for `overlay`'s ink
//! colour). Keep every item `pub(crate)` or narrower; the only intentional ABI
//! surface is `exports.rs`'s `#[no_mangle]` functions -- Phase 51's four
//! (attach/resize/content_rect/detach) plus plan 63-02's three
//! (device_status/simulate_device_lost/recover_device).

pub(crate) mod exports;
pub(crate) mod host;
pub(crate) mod overlay;
pub(crate) mod recovery;
pub(crate) mod sink;
pub(crate) mod state;
pub(crate) mod surface;
