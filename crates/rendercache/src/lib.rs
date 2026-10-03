//! The Rudis timeline render cache — Phase 59, CACHE-03 (identity/invalidation)
//! and CACHE-04 (bounds/location/GC).
//!
//! # The three-layer split, stated verbatim (59-CONTEXT D-39)
//!
//! This subsystem is deliberately cut into three layers, each with exactly one
//! job. The cut exists because the render loop needs `crates/preview`'s
//! `pub(crate)` composite path (`multilayer::cpu_layer_for_spec`,
//! `multilayer::pool_sources`), and widening those to `pub` to buy one caller
//! would export the multilayer resolver's internals across a crate boundary —
//! the opposite of the encapsulation Phases 57 and 58 spent effort building.
//!
//! 1. **`crates/rendercache` (this crate) is a LEAF that owns cache identity,
//!    the segment file format, atomic commit, fail-closed read and the
//!    byte-budget LRU — and NOTHING about compositing.** It mirrors
//!    `crates/proxy` shape for shape, one altitude up: where the proxy cache's
//!    identity is one media file's `(path, mtime_ns, size_bytes)`, a segment's
//!    identity is a hash of *everything a program-time window composites from*.
//! 2. **`crates/preview` owns BOTH the lookup and the render loop**, because
//!    both need the composite path. D-40 pins that to exactly two files — one
//!    reader, one writer — by a source-level scan.
//! 3. **`crates/app-core/src/render_cache_job.rs` owns only the scheduling**:
//!    the `Semaphore(1)`, the encoder admission shared with the proxy worker,
//!    the cancel registry, the import/edit triggers and the poll-only status
//!    getter. It calls INTO preview.
//!
//! Nothing in this crate composites, decodes, or knows what a proxy is. The
//! resolved decode-source answer arrives as plain data ([`key::DecodeAnswerTag`])
//! precisely so that Phase 58 D-16's "one file knows about proxies" property
//! survives this phase.
//!
//! # What lives where
//!
//! * [`key`] — the genuinely new intellectual content: the D-15/D-37 key
//!   material, enumerated from the *types* rather than from a prose list, plus
//!   the fixed segment grid ([`key::SEG_US`]) and its D-07 identity mapping.
//! * [`cache`] — the on-disk substrate, copied in shape from
//!   `crates/proxy/src/cache.rs`: magic + version, the meta written LAST as the
//!   commit marker, temp-then-rename, a read path where every failure is a
//!   silent `None`, and a total-BYTES LRU bound.
//! * [`generate`] — the WRITE path: [`generate::SegmentEncodeSession`] wraps
//!   `engine::RenderCacheEncoder` (the dumb pipe) in this crate's commit
//!   discipline — a temp name, an atomic rename, the meta written LAST, a real
//!   KILL-and-reap cancel, and a prune on every commit. It composites nothing:
//!   the frames arrive from the layer above.
//!
//! # Where the three layers stand, as of this plan
//!
//! ```text
//! layer 1  rendercache  key + cache + generate      <- the write session EXISTS
//! layer 2  preview      read lookup (59-05) and
//!                       the composite-side writer
//!                       that DRIVES a session (59-07)
//! layer 3  app-core     render_cache_job.rs (59-08): the Semaphore(1), the
//!                       shared encoder admission, the cancel registry, the
//!                       import/edit triggers and the poll-only status getter
//! ```
//!
//! The cancel LATCH is deliberately not here. This module guarantees only that
//! [`generate::SegmentEncodeSession::cancel`] leaves no residue; who owns the
//! `AtomicBool`, who raises it and who checks it between frames is layer 3's and
//! layer 2's business respectively (D-23).
//!
//! # The naming hazard, once more
//!
//! `crates/timeline-render` is **not** this. It is the C# shell's Timeline
//! *region* renderer from Phase 52, carrying its own second `wgpu` major. This
//! crate never links it and never extends it.

pub mod cache;
pub mod generate;
pub mod key;

pub use cache::{RENDER_CACHE_DIR_NAME, RENDER_CACHE_MAGIC, RENDER_CACHE_VERSION};
pub use generate::{SegmentCommit, SegmentEncodeSession};
pub use key::{
    segment_bounds, segment_frame_program_us, segment_hash, segment_index_for, ClipKeyMaterial,
    DecodeAnswerTag, MediaIdentity, SegmentKeyInputs, SEG_US,
};
