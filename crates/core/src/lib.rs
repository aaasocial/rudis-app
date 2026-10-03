//! Rudis domain core (Phase 2).
//!
//! Pure domain model + command/undo engine. The Rust backend OWNS this state;
//! the renderer only ever sees serialized snapshots ([`Project`]) and change
//! notifications ([`Patch`]) over IPC. No raw frames, no media I/O, no GPU —
//! and no network: this crate depends only on `serde` and `thiserror`
//! (enforced by `tests/offline_guard.rs`), so the dispatch path can never
//! open a socket.
//!
//! Every mutation goes through [`Store::dispatch`] as a [`Command`]; each
//! command computes its exact inverse at apply time, which is what makes
//! undo/redo correct by construction.

pub mod agent_state;
pub mod canvas;
pub mod command;
pub mod model;
pub mod scene_spec;
pub mod store;
pub mod tools;
pub mod transport;

pub use agent_state::{
    AgentStateView, AnnotationView, ClipView, MediaBinItemView, MediaLibraryView, ShiftRule,
    TimelineDelta, TrackView,
};
pub use canvas::{Annotation, AnnotationShape, AnnotationSpace, CanvasState, NormPoint};
pub use command::{
    is_bundled_font, Command, EntitySnapshot, Patch, PatchKind, MAX_TEXT_CONTENT_LEN,
};
pub use tools::{resolve_track_label, track_label, Tool, ToolError};
pub use model::{
    frame_step_us, is_valid_folder_path, retime_audio_windows, retime_audio_windows_with,
    retime_curve_is_identity,
    retime_source_offset,
    retimed_timeline_len_us, sanitized_retime,
    sample_scalar_track, snap, track_accepts_media, validate_retime, AlphaMode,
    AudioContributor, AudioWindow, PREVIEW_RETIME_AUDIO_WINDOW_US, RETIME_AUDIO_WINDOW_US,
    Clip, ClipCrop, ClipHit,
    ClipTransform, CropValue, Interpolation, Keyframe, KeyframeTrackData, KeyframeTracks,
    MediaBinItem, MediaKind, Playback, PreviewMode, Project, Retime, RetimeCurve, SampledProps,
    SourceAlpha, TextAlign, TextPayload,
    TextStyle, TextStylePatch, Timeline, Track, TrackKind, MAX_KEYFRAMES_PER_TRACK,
    MAX_RETIME_KEYS, MAX_SPEED, MAX_TIMEBASE_FPS, MIN_SPEED,
};
pub use scene_spec::{
    ResolvedBackground, ResolvedElement, ResolvedElementKind, ResolvedSceneSpec, SceneBackground,
    SceneElement, SceneKeyframe, SceneKeyframeTracks, SceneSpec, SceneSpecError, MAX_SCENE_DIM,
    MAX_SCENE_DURATION_SECONDS, MAX_SCENE_ELEMENTS, MAX_SCENE_RASTER_PIXELS_PER_ELEMENT,
    MAX_SCENE_TOTAL_RASTER_PIXELS,
};
pub use store::Store;
pub use transport::TransportCmd;

/// Domain-core error type. Every failure mode is a recoverable `Err` — the
/// core never panics on bad commands, and a rejected command NEVER mutates
/// state (validate-then-apply).
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("track index {0} out of range ({1} tracks exist)")]
    TrackOutOfRange(usize, usize),

    #[error("cannot move clip onto incompatible track: {0}")]
    IncompatibleTrackMedia(String),

    #[error("clip not found: {0}")]
    ClipNotFound(String),

    #[error("media bin item not found: {0}")]
    MediaBinItemNotFound(String),

    #[error("duplicate id: {0}")]
    DuplicateId(String),

    #[error("media bin item {0} is still referenced by clip(s): {1}")]
    MediaBinItemInUse(String, String),

    #[error("invalid clip times: {0}")]
    InvalidTimes(String),

    #[error("media bin item {0} is not previewable video")]
    NotPreviewable(String),

    #[error("no media is loaded in the preview")]
    NoPreviewLoaded,

    #[error("clip {0} already has its audio detached")]
    AlreadyDetached(String),

    #[error("clip {0} references media with no audio to detach")]
    NoAudioToDetach(String),

    #[error("no audio track exists to receive the detached audio clip")]
    NoAudioTrack,

    #[error("annotation not found: {0}")]
    AnnotationNotFound(String),

    #[error("invalid annotation: {0}")]
    InvalidAnnotation(String),

    #[error("invalid project settings: {0}")]
    InvalidSettings(String),

    #[error("clip {0} is not a text clip")]
    NotATextClip(String),

    // ------------------------------------------------------------------
    // Phase 25 (LIB-01): media library folders.
    // ------------------------------------------------------------------
    #[error("invalid media folder path: {0}")]
    InvalidFolderPath(String),

    #[error("media folder not found: {0}")]
    FolderNotFound(String),

    #[error("media folder already exists: {0}")]
    FolderAlreadyExists(String),

    #[error("media folder {0} is not empty")]
    FolderNotEmpty(String),

    #[error("cannot move folder {0} into its own descendant {1}")]
    FolderMoveIntoDescendant(String, String),

    #[error("invalid media display name: {0}")]
    InvalidMediaName(String),
}
