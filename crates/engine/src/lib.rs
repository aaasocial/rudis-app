//! Rudis video engine (Phase 1 spike).
//!
//! Proves the round trip: FFmpeg sidecar decode -> wgpu offscreen composite ->
//! FFmpeg sidecar encode, headlessly verifiable with `cargo test -p engine`.
//!
//! See `.planning/phases/01-engine-round-trip-spike/DECISIONS.md` and
//! `PROVENANCE.md` for the sidecar / preview-surface decision records and
//! licensing provenance.

pub mod annotate;
pub mod audio;
pub mod audio_sync;
pub mod compositor;
pub mod decoder_pool;
pub mod ffmpeg;
// In-process D3D11VA hardware decode for the PREVIEW path (Phase 48, GPU-01 /
// GPU-05 — XTRC-04 clause 2: hardware decode lands ADDITIVELY). The CLI
// sidecar in `ffmpeg.rs` stands beside it, untouched, and still owns export
// (GPU-07). Gated on the default-on `hwdecode` feature (compile-time
// off-switch, plan 48-03) and honoring the RUDIS_DISABLE_HWDECODE runtime
// kill-switch at decoder-open (SPIKE-06's re-isolation path).
#[cfg(all(windows, feature = "hwdecode"))]
pub mod hwdecode;
#[cfg(all(windows, feature = "hwdecode"))]
pub mod import;
// Per-frame YUV→RGB conversion parameters (Phase 48, GPU-02): the CPU builds
// the 3×3 matrix + range offsets from each frame's REAL colorspace/color_range
// tags; the composite shader is a straight matrix multiply. Consumes the
// `FrameColorspace`/`FrameColorRange` types above, so it shares their gate.
#[cfg(all(windows, feature = "hwdecode"))]
pub mod colorspace;
// Coordinated device-lost/TDR recovery (Phase 48, GPU-04): the one detection
// funnel, the 6-step teardown/recreate coordinator (device + swapchain +
// compositor resources + hw frame pool — all four), and the
// RemoveDevice-backed simulate_device_lost injection seam (PROBE_RESULT=works,
// 48-03's measured answer).
#[cfg(all(windows, feature = "hwdecode"))]
pub mod device_lost;
pub mod scene;
pub mod text;
// Runtime VRAM budget for the GPU preview ring (Phase 48, GPU-03): the
// IDXGIAdapter3::QueryVideoMemoryInfo query + budget-change watch thread +
// the pure min-of-two-ceilings ring-depth function. Never the CPU ring's
// ported RING_BUDGET_BYTES constant — GPU-03 prohibits it by name.
#[cfg(all(windows, feature = "hwdecode"))]
pub mod vram_budget;
// Object tracking (Phase 30, TRK-01/TRK-02). Feature-gated so the workspace
// builds without the native OpenCV toolchain. The `track_region` API is
// identical whether the resolved backend is the native `opencv` crate (PRIMARY)
// or a Python sidecar (FALLBACK) — downstream waves are backend-agnostic.
#[cfg(feature = "tracking")]
pub mod tracking;
pub mod whisper;

pub use annotate::{
    draw_arrow, draw_arrow_dashed, draw_lasso, draw_lasso_dashed, draw_marker, draw_stroke,
    draw_stroke_dashed, encode_jpeg_bytes, encode_png_bytes,
};
pub use audio::{AudioOutput, MixSource};
pub use audio_sync::{
    best_lag_us, check_sync_window_cap, SYNC_CONFIDENCE_FLOOR, SYNC_MAX_WINDOW_US,
};
pub use compositor::{
    contain_fit_viewport, AlphaMode, Compositor, CompositeTargetPool, CompositorBirth, Layer,
    LayerCrop, LayerTransform, MixedLayer, PooledTarget, COMPOSITE_TARGET_POOL_DEPTH,
    MAX_MIXED_LAYERS,
};
pub use decoder_pool::{LayerDecoderPool, LayerFrameSource};
#[cfg(all(windows, feature = "hwdecode"))]
pub use hwdecode::{
    acquire_pooled_d3d11_device, clear_pooled_device, force_hw_latch_engage, note_hw_device_reset,
    open_hw_decoder, open_hw_decoder_with,
    HwDecodeSession, HwFailureLatch, HwFrame, HwOpenError, HwOpenStageTimes, TextureDesc,
    DECODER_HEADROOM, HW_DEVICE_CREATE_COUNT, HW_LATCH_RESET_GRACE_MS, HW_OPEN_COUNT,
    HW_OPEN_LAST_STAGES, KILL_SWITCH_ENV, LATCH_THRESHOLD, PROCESS_HW_LATCH,
};
#[cfg(all(windows, feature = "hwdecode"))]
pub use import::{frame_bytes_nv12, import_frame, FrameColorRange, FrameColorspace, GpuFrame};
#[cfg(all(windows, feature = "hwdecode"))]
pub use colorspace::{color_params, ColorParams};
#[cfg(all(windows, feature = "hwdecode"))]
pub use device_lost::{
    assert_same_adapter_luid, inject_forced_device_loss, DeviceLostEvent, DeviceLostSignal,
    PreviewRecovery, RecoveryHooks, RecoveryHostHooks, RecoveryPlan, RecoveryReport,
    RecreatedComponents,
};
pub use scene::{rasterize_ellipse, rasterize_linear_gradient, rasterize_rect, rasterize_solid};
pub use text::{
    offline_font_system, rasterize_text, TextAlign, TextRasterizer, BUNDLED_FONT_FAMILY,
    MAX_RASTER_DIM, MAX_RASTER_PIXELS,
};
#[cfg(all(windows, feature = "hwdecode"))]
pub use vram_budget::{
    gpu_ring_depth, gpu_session_admissible, query_vram_budget, session_pool_vram_bytes,
    LedgerReservation, VramBudgetWatch, VramInfo, VramLedger, GPU_RING_MAX_DEPTH,
    GPU_RING_MIN_DEPTH, POOL_THREAD_SLICES, POOL_VRAM_COUNT_FACTOR, VRAM_BUDGET_FRACTION,
};
pub use whisper::{
    locate_whisper, parse_whisper_json, transcribe_media_window, transcribe_wav, Word,
    WHISPER_MAX_WINDOW_US,
};
pub use ffmpeg::{
    atempo_chain, audio_filter_chain, audio_lead_out_us,
    decode_frame_rgba, decode_frame_rgba_at, decode_frame_rgba_at_scaled,
    decode_frame_rgba_at_seq, decode_frames_rgba_seq,
    encode_from_source, encode_overlay_png_sequence, encode_overlay_prores4444, encoder_available,
    export_encoder_preference, export_nvenc_supports_canvas, export_preferred_encoder,
    extract_wav_for_whisper, frame_step_us, generate_poster, is_mf_video_encoder,
    is_nvenc_video_encoder, locate, probe,
    render_audio_pcm, render_audio_pcm_retimed, render_cache_hw_encoding_enabled,
    render_cache_preferred_encoder, render_cache_scenario, rms, spawn_proxy_encode,
    thread_spawn_counts,
    write_frame_png, write_wav_mono_f32, ExportRunDecoder, FfmpegBinaries, FramePull, Frame,
    MediaInfo, MediaKind,
    ProgressFn, ProxyEncodeChild, RenderCacheEncoder,
    StreamingDecodeSession, VideoEncoder, AUDIO_SAMPLE_RATE, DEFAULT_VIDEO_ENCODER,
    DEV_ENCODER_OVERRIDE_ENV, EXPORT_HW_ENCODER_DISABLE_ENV, EXPORT_NVENC_GOP_SECONDS,
    EXPORT_NVENC_MAX_HEIGHT, EXPORT_NVENC_MAX_WIDTH, EXPORT_NVENC_MIN_HEIGHT,
    EXPORT_NVENC_MIN_WIDTH, EXPORT_NVENC_QP, EXPORT_PREFERRED_ENCODERS,
    MAX_ATEMPO_STAGES, MAX_TEMPO, MIN_TEMPO, PROBE_SPAWN_COUNT,
    PROXY_ENCODE_QUALITY,
    RENDER_CACHE_ENCODE_QUALITY, RENDER_CACHE_HW_ENCODING_ENV, RENDER_CACHE_NVENC_QP,
    RENDER_CACHE_PREFERRED_ENCODERS, RENDER_CACHE_SCENARIO_ENV,
    RETIME_AUDIO_LEAD_OUT_US,
    STREAM_CHANNEL_CAPACITY, STREAM_SPAWN_COUNT,
};

/// Engine error type. Every failure mode is a recoverable `Err` — the engine
/// never panics on bad media input (crash isolation is delegated to the
/// FFmpeg sidecar child process).
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("required binary not found on PATH: {0}")]
    BinaryNotFound(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{tool} exited with status {status}: {stderr}")]
    SidecarFailed {
        tool: String,
        status: i32,
        stderr: String,
    },

    #[error("ffprobe output parse error: {0}")]
    ProbeParse(String),

    #[error("no video stream found in {0}")]
    NoVideoStream(String),

    #[error("no decodable audio/video stream found in {0}")]
    NoMediaStream(String),

    #[error("unexpected pixel buffer size: got {got} bytes, expected {expected}")]
    BadOutputSize { got: usize, expected: usize },

    #[error("invalid time window: out_us ({out_us}) must be greater than in_us ({in_us})")]
    InvalidWindow { in_us: i64, out_us: i64 },

    /// Quick task 260730-x2t (WR-04). A playback tempo bounds BOTH the ffmpeg
    /// `-t` computation and the `atempo` filter chain, and the two fail in
    /// OPPOSITE directions: a non-finite or tiny tempo disables the chain
    /// (`atempo_chain` returns empty) while still SCALING `-t`, which at NaN or
    /// 1e-30 saturates to `i64::MAX` — i.e. "decode to EOF" into an unbounded
    /// `Vec<f32>`. REJECTED rather than sanitized: a wrong tempo is a wrong
    /// export length, and the caller must find out (RT-08).
    #[error(
        "invalid playback tempo: {tempo} (must be finite and within \
         [{min}, {max}])"
    )]
    InvalidTempo { tempo: f32, min: f32, max: f32 },

    #[error("png encode error: {0}")]
    PngEncode(String),

    #[error("jpeg encode error: {0}")]
    JpegEncode(String),

    #[error("gpu error: {0}")]
    Gpu(String),

    #[error("audio output error: {0}")]
    Audio(String),

    #[error("tracking error: {0}")]
    Tracking(String),
}
