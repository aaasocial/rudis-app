//! Rudis playback-proxy support (Phase 58 — PROXY-05).
//!
//! # What this crate is for
//!
//! A *proxy* is a small, all-intra, video-only re-encode of a heavy source file
//! that the PREVIEW path may decode instead of the original. It is never an
//! editing artefact and never a document artefact: nothing here reaches a
//! `.rud` file, the undo stack, or the export renderer. Deleting the whole cache
//! directory is always safe and costs only speed (58-CONTEXT D-14).
//!
//! # Where it sits in the workspace
//!
//! A LEAF crate, exactly like `crates/filmstrip` and `crates/waveform`: it
//! depends on `crates/engine` and on nothing else in this workspace, so the
//! dependency graph stays acyclic while both of its consumers point INTO it:
//!
//! * `crates/preview/src/decode_source.rs` reads it (`cache::read_fresh`) to
//!   answer a proxy instead of the original — plan 58-04. That read path is
//!   stats plus one bounded file read: it never decodes a frame and never
//!   starts a subprocess, because it runs on the resolve path (D-18).
//! * [`generate()`] writes it — plan 58-03 — pairing the engine's proxy-encode
//!   entry point (the encode mechanism landed by 58-01) with this crate's naming,
//!   atomic-commit and eviction discipline, and making cancellation a real KILL
//!   rather than a flag nothing observes (D-11). The app-core job layer above it
//!   (`proxy_job.rs`, plan 58-05) adds only the worker cap, the blocking pool and
//!   the import trigger — no proxy logic of its own.
//!
//! Export deliberately has NO edge to this crate. That absence is what PROXY-04
//! proves (58-CONTEXT D-20).

pub mod cache;
pub mod generate;
pub mod heaviness;

pub use cache::{
    key_for, prune_bytes, proxy_dims, read_fresh, write_meta, ProxyCacheKey, ProxyHit, ProxyMeta,
    MAX_META_FILE_BYTES, MAX_PROXY_CACHE_BYTES, PROXY_CACHE_DIR_NAME, PROXY_CACHE_MAGIC,
    PROXY_CACHE_VERSION, PROXY_FALLBACK_BPP_MILLI, PROXY_LONG_EDGE, PROXY_QUALITY,
};
pub use generate::{generate, GenerateOutcome};
