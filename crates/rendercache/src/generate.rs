//! The segment WRITE path (Phase 59 — CACHE-01/CACHE-04, decisions D-05, D-11
//! inherited, D-23, D-25, D-32).
//!
//! [`SegmentEncodeSession`] is the other half of [`crate::cache`]: the read path
//! already refuses anything that is not a complete, current, correctly-labelled
//! pair, and this module is what makes such a pair — or, on a cancel or a crash,
//! makes sure nothing that could be mistaken for one is left behind.
//!
//! ```text
//! begin ---> push_frame * N ---> finish: rename tmp -> {stem}.seg.mp4
//!   |                                    then write_meta LAST  (COMMIT MARKER)
//!   |                                    then prune_bytes
//!   +-- cancel: KILL the child, reap it, delete the tmp  -> nothing survives
//!   +-- dropped (a crash, an early `?`): the child is killed by the encoder's
//!       own Drop and the tmp is left for the 24 h sweep — never a final name
//! ```
//!
//! # Why this is a SESSION and not a `generate()` call
//!
//! `crates/proxy/src/generate.rs` is this module's reference implementation and
//! it is copied here in every respect that matters — the temp name's exact
//! shape, the atomic rename, the meta-last ordering, the Windows handle-release
//! retry discipline, the post-commit prune, the measured wall-clock. One
//! structural difference, stated plainly rather than glossed:
//!
//! * `proxy::generate` is **file-to-file**. It owns its whole pipeline, so it
//!   can own a poll loop and read a cancel latch inside it.
//! * A cache segment has **no input file**. Its source is composited RGBA frames
//!   produced, one at a time, by the layer above (`crates/preview`, plan 59-07).
//!   There is nothing for this module to poll: the driving loop belongs to the
//!   caller.
//!
//! So the discipline lives in [`SegmentEncodeSession::begin`] /
//! [`SegmentEncodeSession::finish`] / [`SegmentEncodeSession::cancel`] instead
//! of around a single call, and the cancel LATCH is deliberately not here. The
//! writer checks its own `AtomicBool` between frames (59-07) and the registry
//! that owns it is app-core's (59-08); what this module guarantees is narrower
//! and testable: **`cancel()` leaves no residue, and no path through this module
//! can publish a partial file under a final name.**
//!
//! # The commit marker, in one sentence
//!
//! The payload is renamed into `{stem}.seg.mp4` FIRST and
//! [`crate::cache::write_meta`] is called LAST, because
//! [`crate::cache::read_fresh_segment`] stats the meta first — so a crash
//! between those two lines degrades to an orphan payload, which is a MISS and
//! which [`crate::cache::prune_bytes`] sweeps. There is no window in which a
//! reader can see a half-written segment as fresh (D-25, threat T-59-04-01).
//!
//! # The licensing chokepoint, at this crate's boundary
//!
//! There is **no encoder parameter anywhere on this module's surface**, and that
//! omission is the control (CLAUDE.md rule 6, D-05). The encoder arrives from
//! `engine::RenderCacheEncoder`, which resolves it through the engine's ONE
//! cleared path; this module then records the name the child *actually ran with*
//! in the meta — read back off the encoder, never re-resolved from the
//! environment at commit time — so a DEV-override segment is structurally unable
//! to masquerade as a shipped-encoder one. `tests/encoder_license.rs` scans this
//! crate's sources and asserts the door does not exist.
//!
//! # Cost
//!
//! Every commit reports D-32's numbers ([`SegmentCommit`]) and prints a
//! `RENDER-CACHE-GENCOST` line, because 59-10's calibration has no input
//! otherwise and a cost nobody measures is a cost nobody can decide about.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::cache;

/// Windows only releases a killed process's file handle *shortly* after
/// `TerminateProcess` + `WaitForSingleObject` return. Phase 58-01 MEASURED
/// `ERROR_SHARING_VIOLATION` (os error 32) on the instruction after the reap,
/// clearing after 0 ms single-threaded and bounded at 2 s under parallel load —
/// see `proxy::generate`'s constant of the same name, where that race was found.
/// A single best-effort `remove_file` would therefore sometimes leave the very
/// temp file [`SegmentEncodeSession::cancel`] promises is gone.
const HANDLE_RETRY_INTERVAL: Duration = Duration::from_millis(10);
/// See [`HANDLE_RETRY_INTERVAL`]. 2 s, matching 58-01's measured bound.
const HANDLE_RETRY_BUDGET: Duration = Duration::from_secs(2);

/// Marker embedded in the in-flight payload's name.
///
/// It MUST match the marker [`crate::cache::prune_bytes`] sweeps, or an
/// abandoned temp payload would live forever. That coupling is pinned
/// behaviourally by [`tests::the_tmp_payload_name_is_recognisable_to_the_cache_sweep`]
/// rather than by a shared constant, so the test fails if either side drifts.
const TMP_MARKER: &str = ".tmp.";

/// Monotonic per-session nonce, so two concurrent sessions for the SAME identity
/// (which resolve to the same stem) cannot share one temp path and
/// interleave-corrupt it. Mirrors `cache::TMP_COUNTER` and its stated reason.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// What a successful commit reports back — D-23/D-32's measurement, handed to
/// the caller instead of discarded.
///
/// Not a `Result` variant and not a log line: 59-08's job layer surfaces this in
/// its status, and 59-10's calibration consumes it. A generation cost that only
/// ever reached stderr would have to be re-measured by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentCommit {
    /// The committed payload's exact length on disk — the same number the meta
    /// records and the read path cross-checks.
    pub payload_bytes: u64,
    /// Wall-clock milliseconds from [`SegmentEncodeSession::begin`] (i.e.
    /// INCLUDING the encoder-availability probe and the child spawn, which are
    /// real costs) to the completed commit.
    pub wall_ms: u64,
    /// How many frames the session accepted. The caller's own D-07 check: N
    /// frames pushed must be N frames in the segment.
    pub frames: u64,
}

/// One segment's write, from `begin` to either `finish` (commit) or `cancel`
/// (kill, reap, leave nothing).
///
/// Held by the composite-side writer (59-07) for the duration of one segment.
/// It owns the child only indirectly — `engine::RenderCacheEncoder` owns the
/// process; this type owns the **commit marker**, which is the whole of the
/// division of labour between 59-03 and this plan.
///
/// Dropping a session without calling either `finish` or `cancel` is SAFE and
/// its behaviour is pinned: the encoder's own `Drop` kills the child, and the
/// temp payload is left on disk for [`crate::cache::prune_bytes`]'s
/// [`crate::cache::STALE_TMP_AGE`] sweep. It is deliberately NOT deleted on
/// drop, because a `Drop` that touches the filesystem during a panic unwind is a
/// second failure mode stacked on the first; what matters — and what is
/// asserted — is that no file ever reaches a FINAL name.
pub struct SegmentEncodeSession {
    /// 59-03's dumb pipe. It owns the child, the stdin, the stderr pump and the
    /// kill; it knows nothing about names, commits or budgets.
    encoder: engine::RenderCacheEncoder,
    dir: PathBuf,
    stem: String,
    /// Where the encode is actually writing. NEVER the final path — that single
    /// fact is what makes a killed, crashed or failed session unable to leave
    /// anything a later read could mistake for a segment.
    tmp_path: PathBuf,
    final_path: PathBuf,
    seg_index: i64,
    hash: u64,
    canvas_w: u32,
    canvas_h: u32,
    fps: f64,
    started: Instant,
}

/// The in-flight payload's file name for `stem`, unique per session:
/// `{stem}.seg.tmp.{pid}-{nonce}.mp4`.
///
/// Four constraints meet in this one string, and `proxy::generate` learned the
/// fourth the hard way — its finding is inherited here rather than rediscovered:
///
/// 1. **It must still end in `.mp4`.** ffmpeg chooses its muxer from the output
///    file's EXTENSION. The obvious temp shape — appending the marker, exactly
///    as [`crate::cache::write_meta`] does for its JSON — yields
///    `{stem}.seg.mp4.tmp.1234-5`, whose extension is `.1234-5`, and every
///    encode dies with *"Unable to choose an output format"* before writing a
///    byte. So the marker goes in the MIDDLE and the extension stays last.
/// 2. **It must carry `.seg.`**, or [`crate::cache::prune_bytes`] ignores the
///    name entirely (it considers only files carrying that infix) and an
///    abandoned session's output leaks forever.
/// 3. **It must carry the `.tmp.` marker**, or that same sweep — which checks
///    for the marker BEFORE it classifies payloads — would see a payload with no
///    meta and delete it while a LIVE writer still holds it.
/// 4. **It must NOT end in `.seg.mp4`**, or a reader could mistake an in-flight
///    write for a committed payload.
///
/// The extension is derived from [`crate::cache::SEGMENT_PAYLOAD_SUFFIX`] rather
/// than retyped, so a change to the payload suffix carries through here
/// automatically.
pub fn tmp_payload_name(stem: &str) -> String {
    let nonce = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    // ".seg.mp4" -> (".seg", "mp4")
    let (infix, ext) = cache::SEGMENT_PAYLOAD_SUFFIX
        .rsplit_once('.')
        .unwrap_or((".seg", "mp4"));
    format!(
        "{stem}{infix}{TMP_MARKER}{}-{nonce:x}.{ext}",
        std::process::id()
    )
}

/// Delete `path`, retrying briefly while the OS still holds the killed child's
/// handle (see [`HANDLE_RETRY_INTERVAL`]).
///
/// Best-effort: a file that still cannot be removed is left for
/// [`crate::cache::prune_bytes`]'s stale-temp sweep, which is exactly why that
/// sweep exists. Nothing here panics and nothing here returns an error — a
/// failed cleanup costs disk, never correctness.
fn remove_with_retry(path: &Path) {
    let deadline = Instant::now() + HANDLE_RETRY_BUDGET;
    loop {
        if !path.exists() {
            return;
        }
        if std::fs::remove_file(path).is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "render cache: could not delete {} — leaving it for the cache's \
                 stale-temp sweep",
                path.display()
            );
            return;
        }
        std::thread::sleep(HANDLE_RETRY_INTERVAL);
    }
}

/// Rename `from` over `to`, retrying briefly for the same reason
/// [`remove_with_retry`] documents: a just-exited child's handle can outlive its
/// exit status by a few tens of milliseconds on Windows.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let deadline = Instant::now() + HANDLE_RETRY_BUDGET;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(HANDLE_RETRY_INTERVAL);
            }
        }
    }
}

impl SegmentEncodeSession {
    /// Open a session for segment `seg_index` under identity `hash`, rendered at
    /// `canvas_w` x `canvas_h` at `fps`.
    ///
    /// `canvas_w`/`canvas_h` are the FULL project canvas: D-06 forbids writing a
    /// dynamic-resolution-degraded composite to a cache file, and this layer
    /// cannot check that — the caller (59-07) must composite at
    /// `ResLevel::Full`, and the geometry recorded here is what the read path
    /// will compare against, so a degraded render committed under a full-canvas
    /// identity would be a *visible* defect rather than a silent one.
    ///
    /// **There is no encoder parameter, and that omission is the point** — see
    /// the module doc.
    ///
    /// # Refused up front rather than after the encode
    ///
    /// A canvas or cadence that [`crate::cache::SegmentMeta::is_sane`] would
    /// reject can never be committed, because [`crate::cache::write_meta`]
    /// refuses to persist a meta that could never be read back. Discovering that
    /// *after* spending seconds of encoder time would be pure waste, so the same
    /// judgement is made here, before the child is spawned, using the very same
    /// function so the two can never drift.
    pub fn begin(
        dir: &Path,
        seg_index: i64,
        hash: u64,
        canvas_w: u32,
        canvas_h: u32,
        fps: f64,
    ) -> Result<SegmentEncodeSession, engine::EngineError> {
        // D-32: the clock starts BEFORE the spawn, so the encoder-availability
        // probe and the child startup are counted honestly as part of the cost
        // (`proxy::generate`'s rule, same reason).
        let started = Instant::now();

        // The probe meta is a THROWAWAY whose only job is to ask `is_sane` the
        // question. `payload_bytes` is 1 because zero is itself an insanity and
        // the real length is not known yet; every other field is exactly what
        // `finish` will write.
        let probe = cache::SegmentMeta::new(seg_index, hash, canvas_w, canvas_h, fps, 1);
        if !probe.is_sane() {
            return Err(engine::EngineError::SidecarFailed {
                tool: "render-cache segment session".to_string(),
                status: -1,
                stderr: format!(
                    "refusing to open a session for segment {seg_index} at \
                     {canvas_w}x{canvas_h} @ {fps} — the cache could never read such a \
                     segment back, so encoding it would be pure waste"
                ),
            });
        }

        std::fs::create_dir_all(dir)?;

        let stem = cache::file_stem_for(hash, seg_index);
        let final_path = dir.join(cache::payload_file_name(&stem));
        let tmp_path = dir.join(tmp_payload_name(&stem));

        // `SEGMENT_BITRATE_BPS` is still the ONE constant this crate hands the
        // encoder, and since 59-11 it is the DEV-override door's `-b:v` floor
        // rather than the shipped one: on the Media Foundation path the engine
        // emits `-rate_control quality -quality
        // engine::RENDER_CACHE_ENCODE_QUALITY`, and nor does it reach the NVENC
        // branch, which emits `-rc constqp -qp engine::RENDER_CACHE_NVENC_QP`.
        // On neither shipped path does this value reach the command line. The
        // pass-through is kept (not dropped) because the DEV-override branch
        // genuinely needs it, and because nothing outside the engine may choose
        // an encode parameter — including WHICH encoder: `RenderCacheEncoder`
        // resolves that through the engine's own ladder and this call site is
        // deliberately unable to influence it.
        let encoder = engine::RenderCacheEncoder::new(
            &tmp_path,
            canvas_w,
            canvas_h,
            fps,
            cache::SEGMENT_BITRATE_BPS,
        )?;

        Ok(SegmentEncodeSession {
            encoder,
            dir: dir.to_path_buf(),
            stem,
            tmp_path,
            final_path,
            seg_index,
            hash,
            canvas_w,
            canvas_h,
            fps,
            started,
        })
    }

    /// Push one tightly-packed RGBA frame (`canvas_w * canvas_h * 4` bytes) in
    /// presentation order.
    ///
    /// Length-checked by the encoder: `-f rawvideo` carries no framing, so one
    /// short write silently mis-strides every frame after it. An error here
    /// means the sidecar is gone; the caller's answer is to abandon the segment
    /// ([`cancel`](Self::cancel)) and let the range play live (D-21).
    pub fn push_frame(&mut self, rgba: &[u8]) -> Result<(), engine::EngineError> {
        self.encoder.push_frame(rgba)
    }

    /// How many frames this session has accepted so far.
    pub fn frames_pushed(&self) -> u64 {
        self.encoder.frames_pushed()
    }

    /// Where the encode is writing right now. Diagnostics and gates only — a
    /// caller must never read this expecting a playable file, because a file at
    /// this path is by definition incomplete.
    pub fn tmp_path(&self) -> &Path {
        &self.tmp_path
    }

    /// The name the payload will wear if — and only if — this session commits.
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    /// The encoder the child is actually running (post dev-override resolution).
    /// This is what [`finish`](Self::finish) records in the meta.
    pub fn encoder_name(&self) -> &str {
        self.encoder.encoder_name()
    }

    /// Non-blocking poll of the sidecar; `Ok(None)` while it is still running.
    ///
    /// The frame loop uses this to notice a child that died mid-segment instead
    /// of discovering it sixty frames later, and the cancel gate uses it to
    /// prove the kill happened on a LIVE child rather than on an already-dead
    /// one.
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.encoder.try_wait()
    }

    /// Close the encode and COMMIT the segment: rename the payload into its
    /// final name, then write the meta LAST, then enforce the byte budget.
    ///
    /// # The ordering IS the commit marker (D-25, threat T-59-04-01)
    ///
    /// [`crate::cache::read_fresh_segment`] stats the meta FIRST, so a segment
    /// is readable exactly when its `.seg.json` exists. Therefore:
    ///
    /// * the payload reaches its final name first — at which point the entry is
    ///   an ORPHAN, which is a MISS and which `prune_bytes` sweeps;
    /// * the meta lands last — and `write_meta` refuses to commit a meta whose
    ///   payload is absent or whose length disagrees, so the invariant is
    ///   ENFORCED at both ends rather than merely documented;
    /// * a crash anywhere in between degrades to that same sweepable orphan.
    ///
    /// There is no instant at which a reader can see a partial segment as fresh.
    ///
    /// Every failure arm removes what it published, so a failed commit leaves
    /// the directory as it found it. On error the session is consumed and the
    /// child is already reaped by the encoder.
    pub fn finish(self) -> Result<SegmentCommit, engine::EngineError> {
        // Destructured rather than field-by-field because `encoder.finish()`
        // consumes the encoder. There is no `Drop` on this type, so the move is
        // legal and nothing is skipped by it.
        let SegmentEncodeSession {
            encoder,
            dir,
            stem,
            tmp_path,
            final_path,
            seg_index,
            hash,
            canvas_w,
            canvas_h,
            fps,
            started,
        } = self;

        // Read BEFORE the encoder is consumed, and read the encoder's own
        // counters rather than re-deriving them: the meta must describe what
        // actually ran.
        let frames = encoder.frames_pushed();
        let encoder_name = encoder.encoder_name().to_string();

        let payload_bytes = match encoder.finish() {
            Ok(bytes) => bytes,
            Err(e) => {
                // The child is already reaped by `finish`'s own error paths;
                // all that is left is the half-written temp file.
                remove_with_retry(&tmp_path);
                return Err(e);
            }
        };

        // ---- commit, part 1: the payload takes its final name ---------------
        if let Err(e) = rename_with_retry(&tmp_path, &final_path) {
            remove_with_retry(&tmp_path);
            return Err(engine::EngineError::Io(e));
        }

        // ---- commit, part 2: the meta, LAST --------------------------------
        //
        // `payload_bytes` is the length the encoder measured on the file it just
        // finalised; the rename does not change it. It is deliberately NOT
        // re-stat'ed here, so that `write_meta`'s own cross-check (which DOES
        // stat the final path) stays a live gate rather than a tautology.
        let mut meta =
            cache::SegmentMeta::new(seg_index, hash, canvas_w, canvas_h, fps, payload_bytes);
        // The encoder that RAN, taken off the child — not re-resolved from the
        // environment at commit time. If the DEV override changed while this
        // segment was encoding, the meta now disagrees with
        // `cache::effective_encoder()`, `is_sane` fails, and `write_meta`
        // refuses the commit. Failing closed there is the correct outcome: a
        // segment labelled with an encoder that did not produce it is exactly
        // what D-05's stored-encoder compare exists to make impossible.
        meta.encoder = encoder_name;

        if !cache::write_meta(&dir, &stem, &meta) {
            // A payload with no meta is an orphan the sweep would eventually
            // eat. Leaving one deliberately would be worse than useless: it
            // burns disk to buy a permanent MISS.
            remove_with_retry(&final_path);
            return Err(engine::EngineError::SidecarFailed {
                tool: "render-cache segment commit".to_string(),
                status: -1,
                stderr: format!(
                    "the cache refused to commit segment {seg_index} ({canvas_w}x{canvas_h} \
                     @ {fps}, {payload_bytes} bytes, encoder {})",
                    meta.encoder
                ),
            });
        }

        // The byte budget is enforced on EVERY commit, at the layer that just
        // added bytes (`proxy::generate`'s pattern). `write_meta` already prunes
        // on a successful commit; repeating it here is deliberate and cheap —
        // a second pass over an already-bounded directory evicts nothing, and it
        // keeps the bound legible where the growth happens. `dir` is the
        // directory this session was handed and just wrote into, never one this
        // module derived (58-02's threat flag, answered the same way).
        cache::prune_bytes(&dir, cache::MAX_RENDER_CACHE_BYTES);

        let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // D-32: the cost is PRINTED, not hidden. One line per committed segment,
        // the same discipline `proxy_job` applies to its `Created` outcome.
        eprintln!(
            "RENDER-CACHE-GENCOST seg_index={seg_index} seg_frames={frames} \
             wall_ms={wall_ms} bytes={payload_bytes} encoder={}",
            meta.encoder
        );

        Ok(SegmentCommit {
            payload_bytes,
            wall_ms,
            frames,
        })
    }

    /// Abandon the segment for real (D-23): KILL the child, reap it, and delete
    /// the in-flight payload.
    ///
    /// Returns nothing and cannot fail — there is no decision for a caller to
    /// make about a cancellation that has already been decided.
    ///
    /// # Why a kill and not a flag
    ///
    /// The research pitfall `proxy::generate` records applies unchanged: a
    /// single `ffmpeg` invocation has no loop in which to observe a flag, so a
    /// flag-only cancel would return promptly while the encode kept burning the
    /// hardware encoder session playback is entitled to.
    /// `engine::RenderCacheEncoder::kill` carries it out — and kills BEFORE
    /// dropping stdin, because an EOF first would race the sidecar into
    /// finalising a *complete-looking* short segment, which is the one outcome a
    /// cancel must never produce (59-03's finding; do not reorder it).
    ///
    /// # Why the removal retries
    ///
    /// See [`HANDLE_RETRY_INTERVAL`]: the kernel can hold the killed child's
    /// file handle for a measured span after the reap returns, and a single
    /// best-effort `remove_file` would sometimes leave behind the very file this
    /// method promises is gone.
    pub fn cancel(mut self) {
        self.encoder.kill();
        // Drop the encoder (and with it every handle this process holds on the
        // temp file) BEFORE attempting the removal, so the retry loop is only
        // ever waiting on the kernel's post-terminate delay and never on us.
        let SegmentEncodeSession {
            encoder, tmp_path, ..
        } = self;
        drop(encoder);
        remove_with_retry(&tmp_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The temp name must satisfy all four constraints [`tmp_payload_name`]
    /// documents. Three are about the cache sweep; the fourth is about ffmpeg,
    /// and it is the one `proxy::generate` learned by watching every encode fail
    /// before it wrote a byte.
    #[test]
    fn the_tmp_payload_name_is_recognisable_to_the_cache_sweep() {
        let name = tmp_payload_name(&cache::file_stem_for(0xdead_beef_dead_beef, 7));
        assert!(
            name.contains(".seg."),
            "prune_bytes ignores any name without .seg. — {name} would leak forever"
        );
        assert!(
            name.contains(TMP_MARKER),
            "without {TMP_MARKER} the sweep would classify {name} as an orphan payload \
             and delete it while the encoder still holds it"
        );
        assert!(
            !name.ends_with(cache::SEGMENT_PAYLOAD_SUFFIX),
            "an in-flight payload must NOT be readable as a committed payload: {name}"
        );
        assert!(
            !name.ends_with(cache::SEGMENT_META_SUFFIX),
            "and it must not look like a commit marker either: {name}"
        );
        // INHERITED MEASUREMENT: ffmpeg picks its muxer from the EXTENSION. With
        // the marker appended instead of infixed, the extension became
        // `.1234-5` and every encode died with "Unable to choose an output
        // format" before writing a byte.
        assert!(
            name.ends_with(".mp4"),
            "ffmpeg chooses its muxer from the extension — {name} would make every \
             segment encode fail to even open its output"
        );
    }

    /// Two sessions must never collide, even for the same identity — two
    /// concurrent renders of one segment would otherwise share a handle.
    #[test]
    fn tmp_payload_names_are_unique_per_session() {
        let stem = cache::file_stem_for(1, 1);
        assert_ne!(
            tmp_payload_name(&stem),
            tmp_payload_name(&stem),
            "the per-session nonce must make temp names unique"
        );
    }

    /// A name that no reader will ever mistake for a committed pair, checked
    /// from the READER's side rather than by inspecting the string: the stem the
    /// read path derives is not the stem the temp name carries.
    #[test]
    fn a_temp_name_never_derives_the_readers_stem() {
        let stem = cache::file_stem_for(42, 5);
        let tmp = tmp_payload_name(&stem);
        assert_ne!(tmp, cache::payload_file_name(&stem));
        assert_ne!(tmp, cache::meta_file_name(&stem));
    }
}
