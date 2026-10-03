//! Transport commands (Phase 4): preview load / play / pause / seek / step.
//!
//! DECISION — transport is NOT undoable: Ctrl+Z after scrubbing must revert
//! the last EDIT, not teleport the playhead. Playback navigation therefore
//! bypasses the [`crate::Command`]/undo machinery entirely and mutates
//! [`Playback`] through [`crate::Store::transport`], which never touches the
//! undo/redo stacks. Same validate-then-apply guarantee: a rejected transport
//! command leaves state untouched.
//!
//! Serialized adjacently tagged (`{"type": "seek", "data": {...}}`; unit
//! variants are just `{"type": "play"}`) — the same wire shape as `Command`,
//! so the renderer constructs these as plain typed JSON over IPC.

use serde::{Deserialize, Serialize};

use crate::model::{MediaKind, Playback, PreviewMode, Project};
use crate::CoreError;

/// Every transport mutation. Applied via [`crate::Store::transport`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TransportCmd {
    /// Load a MediaBin video item into the Preview: playhead to 0, paused,
    /// duration/fps copied from the item.
    LoadPreview { media_id: String },
    /// Start the frame clock. Restarts from 0 when the playhead sits at the
    /// end (play-after-finish just works).
    Play,
    /// Stop the frame clock (position keeps its value).
    Pause,
    /// Move the playhead to `position_us`, clamped to `[0, duration]`.
    /// Playing state is preserved (scrubbing during playback keeps playing).
    Seek { position_us: i64 },
    /// Nudge the playhead by whole frames (`+1` / `-1` from the arrow keys),
    /// clamped to `[0, duration]`. Frame-stepping is a paused-mode
    /// operation: it always pauses playback.
    Step { delta_frames: i64 },
    /// Frame-clock tick from the play loop: advance the playhead by the
    /// elapsed wall time. No-op when paused. (The dev play loop drives this
    /// from the renderer's requestAnimationFrame; the production play loop
    /// drives it from the native presenter's vsync.
    /// // PENDING WINDOWS VERIFICATION: native-surface clock source.)
    Advance { dt_us: i64 },
    /// Toggle end-of-media behavior (wrap vs pause).
    SetLooping { looping: bool },
    /// Switch the active preview monitor (Source vs Program). Does not change
    /// either playback's position — each tab keeps its own playhead.
    SetPreviewMode { mode: PreviewMode },
}

impl TransportCmd {
    /// Validate against `project`, then apply to `project.playback`. On
    /// `Err`, state is GUARANTEED untouched. Returns the new playback state.
    ///
    /// TIMELINE MODE (Phase 5): when the timeline has clips, the playhead
    /// ranges over the TIMELINE `[0, timeline.duration_us()]` and no loaded
    /// preview media is required — Seek/Play/Advance drive the timeline
    /// preview (`preview_timeline_at`), and timeline bounds take precedence
    /// over the loaded media's (the renderer previews the timeline whenever
    /// it has clips). With an empty timeline, the Phase 4 single-media
    /// semantics are unchanged. `Step` still requires a loaded media: the
    /// frame-step size comes from the loaded item's fps (frame-accurate
    /// timeline stepping is Phase 6's rational-timebase concern).
    pub(crate) fn apply(&self, project: &mut Project) -> Result<Playback, CoreError> {
        // Load / mode-switch commands first — they may change the active tab.
        match self {
            TransportCmd::LoadPreview { media_id } => {
                let item = project
                    .media_bin
                    .iter()
                    .find(|m| m.id == *media_id)
                    .ok_or_else(|| CoreError::MediaBinItemNotFound(media_id.clone()))?;
                if item.media_kind != MediaKind::Video {
                    return Err(CoreError::NotPreviewable(media_id.clone()));
                }
                // Loads into the SOURCE monitor and switches to it — the Timeline
                // (Program) playhead is deliberately untouched.
                project.source_playback = Playback {
                    loaded_media_id: Some(item.id.clone()),
                    playing: false,
                    position_us: 0,
                    duration_us: item.duration_us,
                    fps: item.fps,
                    looping: project.source_playback.looping,
                };
                project.preview_mode = PreviewMode::Source;
                return Ok(project.source_playback.clone());
            }
            TransportCmd::SetPreviewMode { mode } => {
                project.preview_mode = *mode;
                return Ok(match project.preview_mode {
                    PreviewMode::Source => project.source_playback.clone(),
                    PreviewMode::Program => {
                        project.playback.duration_us = project.timeline.duration_us();
                        project.playback.clone()
                    }
                });
            }
            _ => {}
        }

        // Play/Pause/Seek/Step/Advance/SetLooping operate on the ACTIVE tab's
        // playback with that tab's duration + semantics.
        let timeline_dur = project.timeline.duration_us();
        let mode = project.preview_mode;
        let (pb, dur): (&mut Playback, i64) = match mode {
            PreviewMode::Source => {
                let d = project.source_playback.duration_us;
                (&mut project.source_playback, d)
            }
            PreviewMode::Program => {
                // Keep the program playhead's duration synced to the timeline.
                project.playback.duration_us = timeline_dur;
                (&mut project.playback, timeline_dur)
            }
        };

        // "Something to preview": a loaded clip (Source) or timeline content
        // (Program). Play/Seek/Advance require it; nothing-loaded ops error.
        let ready = match mode {
            PreviewMode::Source => pb.loaded_media_id.is_some(),
            PreviewMode::Program => dur > 0,
        };

        match self {
            TransportCmd::Play => {
                if !ready {
                    return Err(CoreError::NoPreviewLoaded);
                }
                if pb.position_us >= dur.max(0) {
                    pb.position_us = 0; // play-after-finish restarts
                }
                pb.playing = true;
            }
            TransportCmd::Pause => pb.playing = false,
            TransportCmd::Seek { position_us } => {
                if !ready {
                    return Err(CoreError::NoPreviewLoaded);
                }
                pb.position_us = (*position_us).clamp(0, dur.max(0));
            }
            TransportCmd::Step { delta_frames } => {
                // Frame stepping needs an fps source (the loaded item). The
                // program playback has none, so timeline frame-stepping errors
                // for now (deferred; matches pre-split behavior).
                if pb.loaded_media_id.is_none() {
                    return Err(CoreError::NoPreviewLoaded);
                }
                let delta = delta_frames.saturating_mul(pb.frame_step_us());
                pb.position_us = pb.position_us.saturating_add(delta).clamp(0, dur.max(0));
                pb.playing = false;
            }
            TransportCmd::Advance { dt_us } => {
                if !ready {
                    return Err(CoreError::NoPreviewLoaded);
                }
                pb.advance_with_duration(*dt_us, dur.max(0));
            }
            TransportCmd::SetLooping { looping } => pb.looping = *looping,
            // Handled above.
            TransportCmd::LoadPreview { .. } | TransportCmd::SetPreviewMode { .. } => unreachable!(),
        }
        Ok(pb.clone())
    }
}
