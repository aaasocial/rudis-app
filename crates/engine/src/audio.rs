//! Phase 9 Wave 4 — live preview-audio OUTPUT via `cpal`.
//!
//! [`AudioOutput`] plays a clip's audio through the OS default output device
//! (WASAPI / CoreAudio / ALSA, selected internally by cpal). The PCM comes from
//! the SAME [`crate::render_audio_pcm`] the Phase 7 export mix consumes — "one
//! path serves preview and export", now extended to live audio.
//!
//! **Clock semantics (audio-master).** [`AudioOutput::samples_consumed`] counts
//! output frames handed to the device; [`AudioOutput::elapsed_us`] turns that
//! into wall-clock microseconds since playback began. A sound card drains its
//! buffer at a fixed real rate whether we filled it with real audio or (on
//! underrun) silence, so this IS a true hardware clock — the present thread
//! paces VIDEO off it (research §6, Assumption A1) so audio and video stay
//! locked without accumulating drift.
//!
//! **Real-time discipline.** The cpal data callback is a hard real-time context
//! and MUST NOT block (cpal's docs are explicit). It only pulls already-rendered
//! PCM from a bounded channel and writes silence into any slots the producer
//! hasn't filled yet (underrun) — it never renders, allocates-to-grow, or locks.
//! A separate PRODUCER thread renders `render_audio_pcm` in ~1s windows ahead of
//! the playhead and pushes them across the bounded channel (backpressure: the
//! producer blocks on a full channel, never the audio callback).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::ffmpeg::{render_audio_pcm, AUDIO_SAMPLE_RATE};
use crate::EngineError;

/// Audio rendered ahead of the playhead in windows this long (microseconds).
/// Audio is rendered ahead of the playhead in windows this long (microseconds).
/// 2s keeps the per-window ffmpeg spawn RATE low during playback (one spawn per
/// ~2s of audio, not per second) while the first window still decodes quickly
/// enough to start audio with low latency. All rendering is on the producer
/// thread — never the present thread — so window size never stalls video.
const CHUNK_US: i64 = 2_000_000;

/// How many rendered PCM windows may sit in the channel before the producer
/// blocks (backpressure). ~6s of lookahead at 2s windows — bounds memory while
/// tolerating scheduling jitter on the producer thread.
const CHUNK_CAPACITY: usize = 3;

/// PLAY-08 (plan 57-05): how many times an OS audio output device has been
/// opened in this process — incremented at the top of BOTH [`AudioOutput::start`]
/// and [`AudioOutput::start_mix`], the only two functions that build a `cpal`
/// output stream.
///
/// This is the test-observable truth behind "the mix rebuilt". Every open costs
/// a real WASAPI device acquisition (0.27-0.39 s, measured in 49.1), and the
/// preview mix-window formula in `preview::resolve_program_audio` used to force
/// one at EVERY clip cut. Counting opens is the only way to score that fix on
/// the real pipeline — a frame-timing measurement can be moved by a dozen other
/// things, but this counter can only move when a device was actually reopened.
///
/// `Relaxed` throughout: it is a monotonic diagnostic counter, never a
/// synchronisation edge. Read it as a DELTA around a span (snapshot, act,
/// snapshot) — the absolute value carries every open since process start,
/// including other tests in the same binary.
pub static AUDIO_STREAM_OPENS: AtomicU64 = AtomicU64::new(0);

/// A live audio playback stream feeding the OS output device from
/// `render_audio_pcm`, exposing an audio-samples-consumed clock for A/V pacing.
///
/// `cpal::Stream` is `!Send` on some backends (WASAPI included), so `AudioOutput`
/// is `!Send` too: it must be constructed and held on ONE thread (the present
/// thread, in the Rudis app). This is by design — no `unsafe impl Send`.
pub struct AudioOutput {
    /// The live output stream. `Option` so `Drop` can drop it BEFORE joining the
    /// producer (dropping the stream releases the callback's channel receiver,
    /// which unblocks a producer parked in `send`).
    stream: Option<cpal::Stream>,
    /// Output frames handed to the device so far (the hardware clock's tick).
    samples_consumed: Arc<AtomicU64>,
    /// The device's actual output sample rate (may differ from 48 kHz; the
    /// producer resamples 48 kHz source PCM to match).
    sample_rate: u32,
    /// Signals the producer thread to stop rendering promptly on drop.
    stop: Arc<AtomicBool>,
    producer: Option<JoinHandle<()>>,
}

/// One audio contribution for [`AudioOutput::start_mix`] — the live twin of
/// `rudis_core::AudioContributor` (kept a plain engine struct so `engine` needs
/// no `rudis_core` dependency). Render `path`'s audio over `[in_us, out_us)` at
/// `volume`, placed on the timeline at `timeline_start_us`.
pub struct MixSource {
    pub path: PathBuf,
    pub timeline_start_us: i64,
    pub in_us: i64,
    pub out_us: i64,
    pub volume: f32,
    /// Constant playback tempo for THIS window (quick task 260730-x2t).
    /// 1.0 = un-retimed, which is the byte-identical pre-retime path. Core
    /// pre-segments a speed RAMP into constant-tempo windows
    /// (`rudis_core::retime_audio_windows`) so the engine never needs the
    /// integral — inside one window the timeline↔source map is LINEAR.
    pub tempo: f32,
    /// The EXACT timeline length this window must occupy. `atempo`'s output
    /// length is only APPROXIMATE, so the mixer places and TRUNCATES to this,
    /// never to the returned PCM length (RT-05).
    pub out_len_us: i64,
}

impl MixSource {
    /// A plain un-retimed contributor: `tempo` 1.0 and a timeline length equal
    /// to the source span. The shape every pre-retime call site built.
    pub fn plain(path: PathBuf, timeline_start_us: i64, in_us: i64, out_us: i64, volume: f32) -> Self {
        Self {
            path,
            timeline_start_us,
            in_us,
            out_us,
            volume,
            tempo: 1.0,
            out_len_us: out_us - in_us,
        }
    }
}

/// Build + start the cpal output stream that [`AudioOutput::start_mix`] drives
/// with its producer thread. The real-time data callback (pull mono from the
/// bounded channel, fan across device channels, advance the audio-master clock
/// once real samples flow) matches [`AudioOutput::start`]'s exactly.
fn spawn_output_stream() -> Result<
    (
        cpal::Stream,
        Arc<AtomicU64>,
        u32,
        std::sync::mpsc::SyncSender<Vec<f32>>,
        Arc<AtomicBool>,
    ),
    EngineError,
> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| EngineError::Audio("no default audio output device".to_string()))?;
    let supported = pick_output_config(&device)?;
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.config();

    let (tx, rx) = sync_channel::<Vec<f32>>(CHUNK_CAPACITY);
    let samples_consumed = Arc::new(AtomicU64::new(0));
    let sc = samples_consumed.clone();

    let mut cur: Vec<f32> = Vec::new();
    let mut pos: usize = 0;
    let mut started = false;
    let data_cb = move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
        let frames = out.len() / channels.max(1);
        for f in 0..frames {
            let sample = loop {
                if pos < cur.len() {
                    let s = cur[pos];
                    pos += 1;
                    started = true;
                    break s;
                }
                match rx.try_recv() {
                    Ok(next) if !next.is_empty() => {
                        cur = next;
                        pos = 0;
                    }
                    _ => break 0.0f32,
                }
            };
            let base = f * channels;
            for c in 0..channels {
                out[base + c] = sample;
            }
        }
        if started {
            sc.fetch_add(frames as u64, Ordering::Relaxed);
        }
    };
    let err_cb = |err| eprintln!("audio: output stream error: {err}");
    let stream = device
        .build_output_stream(config, data_cb, err_cb, None)
        .map_err(|e| EngineError::Audio(format!("build_output_stream: {e}")))?;
    stream
        .play()
        .map_err(|e| EngineError::Audio(format!("stream.play: {e}")))?;
    let stop = Arc::new(AtomicBool::new(false));
    Ok((stream, samples_consumed, sample_rate, tx, stop))
}

impl AudioOutput {
    /// Start playing `path`'s audio over `[start_us, end_us)` at gain `volume`
    /// through the default output device. Returns IMMEDIATELY — all decoding
    /// happens on a producer thread, never on the caller (the present thread must
    /// not stall on an ffmpeg decode). Audio begins as soon as the producer's
    /// first window lands; the clock does not advance until then (see the
    /// callback's `started` gating), so A/V sync stays exact at startup.
    ///
    /// A source with no audio stream (empty `render_audio_pcm`) is VALID input:
    /// the stream is still built and plays silence, matching `render_audio_pcm`'s
    /// "empty vec = silence, not an error" contract.
    pub fn start(
        path: &Path,
        start_us: i64,
        end_us: i64,
        volume: f32,
    ) -> Result<Self, EngineError> {
        AUDIO_STREAM_OPENS.fetch_add(1, Ordering::Relaxed); // PLAY-08 counter
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| EngineError::Audio("no default audio output device".to_string()))?;
        let supported = pick_output_config(&device)?;
        let sample_rate = supported.sample_rate(); // cpal 0.18: SampleRate = u32
        let channels = supported.channels() as usize;
        let config: cpal::StreamConfig = supported.config();

        let (tx, rx) = sync_channel::<Vec<f32>>(CHUNK_CAPACITY);

        let samples_consumed = Arc::new(AtomicU64::new(0));
        let sc = samples_consumed.clone();

        // The real-time consumer: pull mono samples from the channel, fan them
        // out across the device's channels, and advance the clock. It NEVER
        // blocks or renders (both would violate cpal's real-time-callback rule) —
        // an underrun or end-of-stream just writes silence.
        //
        // Clock gating: `started` flips true the moment the callback emits its
        // FIRST real decoded sample. Frames are only counted once `started` — so
        // the pre-roll silence before the producer's first window arrives does
        // NOT advance the clock (this keeps A/V sync exact at startup: the
        // audio-master video pacing holds frame 0 until audio truly begins). Once
        // started, EVERY frame counts (mid-stream underrun silence included) —
        // the sound card keeps draining in real time regardless.
        let mut cur: Vec<f32> = Vec::new();
        let mut pos: usize = 0;
        let mut started = false;
        let data_cb = move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let frames = out.len() / channels.max(1);
            for f in 0..frames {
                let sample = loop {
                    if pos < cur.len() {
                        let s = cur[pos];
                        pos += 1;
                        started = true; // emitting real decoded audio now
                        break s;
                    }
                    match rx.try_recv() {
                        Ok(next) if !next.is_empty() => {
                            cur = next;
                            pos = 0;
                        }
                        // Pre-roll, underrun, or channel closed → silence.
                        _ => break 0.0f32,
                    }
                };
                let base = f * channels;
                for c in 0..channels {
                    out[base + c] = sample;
                }
            }
            if started {
                sc.fetch_add(frames as u64, Ordering::Relaxed);
            }
        };

        let err_cb = |err| eprintln!("audio: output stream error: {err}");
        let stream = device
            .build_output_stream(config, data_cb, err_cb, None)
            .map_err(|e| EngineError::Audio(format!("build_output_stream: {e}")))?;
        stream
            .play()
            .map_err(|e| EngineError::Audio(format!("stream.play: {e}")))?;

        let stop = Arc::new(AtomicBool::new(false));

        // Producer: render the WHOLE range in `CHUNK_US` windows ahead of the
        // playhead, entirely OFF the calling (present) thread — `start` returns
        // immediately and NEVER blocks video on an ffmpeg decode. A source with no
        // audio stream (empty render) simply produces no samples → the callback
        // plays silence (valid input, no panic). One render_audio_pcm call per
        // window (the SAME sum-mix source export uses); the bounded channel gives
        // backpressure so a whole long clip is not buffered at once.
        let producer = {
            let stop2 = stop.clone();
            let path2 = path.to_path_buf();
            Some(std::thread::spawn(move || {
                let mut chunk_start = start_us;
                while chunk_start < end_us {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    let chunk_end = (chunk_start + CHUNK_US).min(end_us);
                    let pcm = match render_audio_pcm(&path2, chunk_start, chunk_end, volume) {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!(
                                "audio producer: render {chunk_start}..{chunk_end} of {}: {e}",
                                path2.display()
                            );
                            break;
                        }
                    };
                    if pcm.is_empty() {
                        break; // no audio stream / range past end → silence
                    }
                    let out = resample_linear(&pcm, AUDIO_SAMPLE_RATE, sample_rate);
                    // Blocks on a full channel (backpressure); Err = receiver
                    // (stream) dropped → stop.
                    if tx.send(out).is_err() {
                        break;
                    }
                    chunk_start = chunk_end;
                }
            }))
        };

        Ok(Self {
            stream: Some(stream),
            samples_consumed,
            sample_rate,
            stop,
            producer,
        })
    }

    /// Start a MIX of multiple audio contributors over the timeline range
    /// `[tl_start_us, tl_end_us)` — the LIVE twin of the Phase-7 export sum-mix
    /// (`build_export_audio_wav`). Each `CHUNK_US` window sums every contributor
    /// overlapping it (rendered at its own source offset + volume) into one
    /// buffer, so DETACHED audio and any audio-track clips play in preview — not
    /// just the active video clip's own audio (the detach-audio-silent fix). A
    /// buffer is sent for EVERY window (silent ones included) so the audio-master
    /// clock keeps advancing across gaps.
    pub fn start_mix(
        sources: Vec<MixSource>,
        tl_start_us: i64,
        tl_end_us: i64,
    ) -> Result<Self, EngineError> {
        AUDIO_STREAM_OPENS.fetch_add(1, Ordering::Relaxed); // PLAY-08 counter
        let (stream, samples_consumed, sample_rate, tx, stop) = spawn_output_stream()?;
        let producer = {
            let stop2 = stop.clone();
            Some(std::thread::spawn(move || {
                // ONE `has_audio` probe per DISTINCT path, hoisted out of the
                // chunk loop (live-UAT bug
                // `retime-live-uat-frontend-mirror-undo-audio`, symptom 3).
                // `render_audio_pcm_retimed` probes on every call just to
                // answer `has_audio`; that is ~50 % of its cost (MEASURED:
                // probe ~110-130 ms vs render ~90-120 ms) and a RAMPED
                // contributor calls it once per staircase window per 2 s chunk,
                // inside a hard real-time budget. The answer cannot change
                // mid-playback — a source either has an audio stream or it does
                // not — so probing once per path per mix is both cheaper and
                // exactly as correct. A probe ERROR is treated as "no audio"
                // for that path, which is the same observable outcome the
                // per-call `Err` produced (the mixer `continue`d past it).
                let mut has_audio: Vec<(PathBuf, bool)> = Vec::new();
                for src in &sources {
                    if has_audio.iter().any(|(p, _)| *p == src.path) {
                        continue;
                    }
                    let answer = match crate::ffmpeg::probe(&src.path) {
                        Ok(info) => info.has_audio,
                        Err(e) => {
                            eprintln!("audio mix: probe {}: {e}", src.path.display());
                            false
                        }
                    };
                    has_audio.push((src.path.clone(), answer));
                }
                let path_has_audio = |p: &Path| -> bool {
                    has_audio
                        .iter()
                        .find(|(hp, _)| hp == p)
                        .map(|(_, a)| *a)
                        .unwrap_or(false)
                };

                let mut w_start = tl_start_us;
                while w_start < tl_end_us {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    let w_end = (w_start + CHUNK_US).min(tl_end_us);
                    let win_us = (w_end - w_start).max(0);
                    let n =
                        ((win_us as f64 / 1_000_000.0) * AUDIO_SAMPLE_RATE as f64).ceil() as usize;
                    let mut mix = vec![0.0f32; n];
                    for src in &sources {
                        // Retime (quick task 260730-x2t): the contributor's
                        // TIMELINE extent is `out_len_us`, NOT its source span
                        // — a 4 s source range at 2x occupies 2 s here. For an
                        // un-retimed window `out_len_us == out_us - in_us` and
                        // `tempo == 1.0`, so everything below collapses to
                        // exactly the code that ran before retime existed.
                        let src_tl_end = src.timeline_start_us + src.out_len_us;
                        let os = w_start.max(src.timeline_start_us);
                        let oe = w_end.min(src_tl_end);
                        if os >= oe {
                            continue; // this contributor does not overlap this window
                        }
                        // Within ONE constant-tempo window the map is LINEAR,
                        // so slicing the 2 s chunk out of it is a multiply —
                        // no integral, which is why `MixSource` can stay a
                        // plain engine struct with no `rudis_core` dependency.
                        let tempo = if src.tempo.is_finite() && src.tempo > 0.0 {
                            src.tempo
                        } else {
                            1.0
                        };
                        let rs = src.in_us
                            + (((os - src.timeline_start_us) as f64 * tempo as f64) as i64);
                        let re = (src.in_us
                            + (((oe - src.timeline_start_us) as f64 * tempo as f64) as i64))
                            .min(src.out_us);
                        if re <= rs {
                            continue;
                        }
                        // O-3 DEFERRED (Phase 19 Plan 04, COMP-04): this live
                        // preview mix renders each contributor at its STATIC
                        // `src.volume` and does NOT yet apply a volume-keyframe
                        // ENVELOPE — so a keyframed fade is heard flat in preview
                        // but is correct in the shipped EXPORT (build_export_audio_wav
                        // already samples the envelope). Parity would apply
                        // `rudis_core::sample_scalar_track(volume_track, clip_rel_us,
                        // project_fps)` per output sample here — but `MixSource` is a
                        // plain engine struct with NO `rudis_core` dependency (see its
                        // doc), so wiring it means either an engine->core dep or
                        // duplicating the sampler (forbidden: ONE shared sampler). All
                        // Phase-19 SCs are visual/export; the audible artifact is
                        // correct. Recorded in 19-04-SUMMARY.md + 19-VALIDATION.md.
                        //
                        // RETIME IS DELIBERATELY *NOT* GIVEN THE SAME TREATMENT
                        // (quick task 260730-x2t). The volume-envelope gap above
                        // is an explicitly accepted preview/export difference;
                        // retime is NOT allowed to inherit that precedent. A
                        // wrong-SPEED preview is a far larger artifact than a
                        // flat volume envelope — the picture would drift against
                        // the sound and against the export — and CONTEXT.md locks
                        // preview/export agreement for this feature. So live
                        // preview audio time-stretches FOR REAL, through the
                        // `MixSource.tempo` channel, using the SAME
                        // `render_audio_pcm_retimed` the export path uses. This
                        // paragraph exists so the next reader does not assume the
                        // volume-envelope deferral extended to speed.
                        // Lead-out (RT-05): a RETIMED contributor's slice asks
                        // for a little extra SOURCE so codec-frame granularity
                        // and atempo's approximate output length cannot leave a
                        // silent hole at a chunk seam; the extra is discarded by
                        // the truncation below. An UN-retimed contributor gets
                        // no lead-out, so its render call is literally the one
                        // that ran before retime existed.
                        let re_ext = if tempo == 1.0
                            && src.out_len_us == src.out_us - src.in_us
                        {
                            re
                        } else {
                            re + crate::ffmpeg::audio_lead_out_us(tempo)
                        };
                        let pcm = match crate::ffmpeg::render_audio_pcm_retimed_known_audio(
                            &src.path,
                            rs,
                            re_ext,
                            src.volume,
                            tempo,
                            path_has_audio(&src.path),
                        ) {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!("audio mix: render {} {rs}..{re}: {e}", src.path.display());
                                continue; // one bad contributor never silences the whole mix
                            }
                        };
                        if pcm.is_empty() {
                            continue;
                        }
                        // TRUNCATE to the exact expected sample count (RT-05).
                        // `atempo`'s output length is APPROXIMATE, so trusting
                        // `pcm.len()` would let each window's error accumulate
                        // into audible drift against the picture. The expected
                        // count comes from the TIMELINE span this slice covers.
                        let expect = (((oe - os) as f64 / 1_000_000.0)
                            * AUDIO_SAMPLE_RATE as f64)
                            .round() as usize;
                        let take = pcm.len().min(expect);
                        let off = (((os - w_start) as f64 / 1_000_000.0) * AUDIO_SAMPLE_RATE as f64)
                            .round() as usize;
                        for (i, &s) in pcm.iter().take(take).enumerate() {
                            let idx = off + i;
                            if idx >= mix.len() {
                                break;
                            }
                            mix[idx] += s;
                        }
                    }
                    let out = resample_linear(&mix, AUDIO_SAMPLE_RATE, sample_rate);
                    if tx.send(out).is_err() {
                        break;
                    }
                    w_start = w_end;
                }
            }))
        };
        Ok(Self {
            stream: Some(stream),
            samples_consumed,
            sample_rate,
            stop,
            producer,
        })
    }

    /// Output frames delivered to the device so far — the audio-master clock's
    /// raw tick count. `frames / sample_rate` seconds of audio have played.
    pub fn samples_consumed(&self) -> u64 {
        self.samples_consumed.load(Ordering::Relaxed)
    }

    /// The device's output sample rate (Hz).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Microseconds of audio played since `start` — the value the present thread
    /// paces video against and the drift harness compares to the video clock.
    /// Device-rate-correct: `samples_consumed` counts output frames at
    /// `sample_rate`, so this is real elapsed time even when the device is not
    /// running at 48 kHz.
    pub fn elapsed_us(&self) -> i64 {
        let sr = self.sample_rate.max(1) as i128;
        (self.samples_consumed() as i128 * 1_000_000 / sr) as i64
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        // Signal the producer to stop, then DROP THE STREAM FIRST so the
        // callback's channel receiver is released — that unblocks a producer
        // parked in a full-channel `send`, letting the join below complete
        // deterministically (never deadlock).
        self.stop.store(true, Ordering::Relaxed);
        drop(self.stream.take());
        if let Some(h) = self.producer.take() {
            let _ = h.join();
        }
    }
}

/// Resolve a usable f32 output config. We use the device's DEFAULT output
/// config (its mix format) rather than a hand-picked channel/rate combo, because
/// WASAPI shared mode only accepts the device's own mix format — a "nicer" combo
/// (e.g. forcing mono/48 kHz) is rejected with "not supported in shared mode".
/// The callback fans mono samples across whatever channel count the mix format
/// has, and the producer resamples 48 kHz source PCM to its sample rate, so any
/// f32 mix format works. Errors if the default format is not f32 (Rudis renders
/// f32 PCM; a device defaulting to i16-only output is not supported this
/// milestone).
fn pick_output_config(
    device: &cpal::Device,
) -> Result<cpal::SupportedStreamConfig, EngineError> {
    let default = device
        .default_output_config()
        .map_err(|e| EngineError::Audio(format!("default_output_config: {e}")))?;
    if default.sample_format() != cpal::SampleFormat::F32 {
        return Err(EngineError::Audio(format!(
            "default output device uses {:?} samples; live preview-audio needs an \
             f32 output mix format",
            default.sample_format()
        )));
    }
    Ok(default)
}

/// Linear-interpolation resample of mono `src` from `from` Hz to `to` Hz. A
/// no-op (clone) when the rates match — the common Windows/48 kHz case. Cheap
/// and good enough for preview; a chunk boundary introduces at most one sample
/// of interpolation error, inaudible.
fn resample_linear(src: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || src.is_empty() {
        return src.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = ((src.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 / ratio;
        let idx = pos.floor() as usize;
        let frac = (pos - idx as f64) as f32;
        let a = src.get(idx).copied().unwrap_or(0.0);
        let b = src.get(idx + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}
