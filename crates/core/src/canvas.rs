//! Canvas annotation domain types (Phase 13, CANV-01).
//!
//! Fills the `Project.canvas` extensibility seam ARCHITECTURE.md named in
//! Phase 2. Annotations are REAL, inspectable, serde-serializable data — a
//! point list, never a bitmap (SC-1). Pure domain types: this module uses
//! only `serde` (the crate's offline/purity guarantee is unchanged; see
//! `tests/offline_guard.rs`).
//!
//! Validation (coordinate clamping, structural rejection, label truncation)
//! deliberately does NOT live here — it lives in `Command::apply`
//! (command.rs), mirroring how `Clip` validation lives in `AddClip`'s
//! handler, not on `Clip` itself.

use serde::{Deserialize, Serialize};

/// Normalized [0.0, 1.0] point relative to the video FRAME CONTENT (never the
/// outer stage, never letterbox bars). Clamped into range by
/// `Command::apply` — never trust frontend-computed math.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormPoint {
    pub x: f64,
    pub y: f64,
}

impl NormPoint {
    pub fn clamped(x: f64, y: f64) -> Self {
        Self {
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnnotationShape {
    Stroke { points: Vec<NormPoint> },
    Lasso { points: Vec<NormPoint> },
    Arrow { start: NormPoint, end: NormPoint },
    Label { position: NormPoint, text: String },
}

impl AnnotationShape {
    /// Static kind label reused by agent_state's AnnotationView/render_compact.
    pub fn kind_label(&self) -> &'static str {
        match self {
            AnnotationShape::Stroke { .. } => "stroke",
            AnnotationShape::Lasso { .. } => "lasso",
            AnnotationShape::Arrow { .. } => "arrow",
            AnnotationShape::Label { .. } => "label",
        }
    }
}

/// Which drawing surface an annotation belongs to (Phase 14.2, D-06).
/// A single discriminator (NOT a second Vec, NOT derived from
/// `linked_range_us` — a Preview mark can legitimately have
/// `linked_range_us: None`) so every existing Command stays generic.
/// Mirrors `PreviewMode`'s plain-discriminator precedent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationSpace {
    /// Existing behavior: drawn on the Preview overlay + baked into the
    /// frame vision snapshot; carries `linked_range_us`.
    #[default]
    FrameLinked,
    /// New (D-05): project-global ideas surface. Drawn ONLY onto the
    /// whiteboard rasterization; NEVER onto the live Preview or a video
    /// frame. `linked_range_us` is `None` for these.
    Whiteboard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    /// Opaque id, minted by the FRONTEND at draw-finish time (mirrors
    /// AddClip/AddMediaBinItem's caller-supplied-id convention).
    pub id: String,
    pub shape: AnnotationShape,
    /// Captured by the FRONTEND from the active monitor's playhead at
    /// draw-finish time — a POINT range (start == end) until a future phase
    /// adds an explicit range picker. `None` if playback position was
    /// unavailable. The backend does not derive or validate this beyond
    /// Option<> passthrough.
    pub linked_range_us: Option<(i64, i64)>,
    /// Phase 14.2: which surface this mark lives on. `#[serde(default)]`
    /// means pre-14.2 snapshots (no key) load as `FrameLinked` — the
    /// exact backward-compat pattern used for `Project.canvas` and
    /// `AnnotationView.range_*`.
    #[serde(default)]
    pub space: AnnotationSpace,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CanvasState {
    pub annotations: Vec<Annotation>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Project;

    fn p(x: f64, y: f64) -> NormPoint {
        NormPoint { x, y }
    }

    /// Serialize then deserialize back to the identical Rust value.
    fn round_trip(shape: AnnotationShape) -> AnnotationShape {
        let json = serde_json::to_string(&shape).expect("serialize shape");
        serde_json::from_str(&json).expect("deserialize shape")
    }

    #[test]
    fn stroke_shape_serde_round_trips() {
        let shape = AnnotationShape::Stroke {
            points: vec![p(0.1, 0.2), p(0.3, 0.4), p(0.5, 0.6)],
        };
        assert_eq!(round_trip(shape.clone()), shape);
        // Wire form is internally tagged (`"kind": "stroke"`).
        let json = serde_json::to_value(&shape).unwrap();
        assert_eq!(json["kind"], "stroke");
    }

    #[test]
    fn lasso_shape_serde_round_trips() {
        let shape = AnnotationShape::Lasso {
            points: vec![p(0.2, 0.2), p(0.4, 0.2), p(0.4, 0.4)],
        };
        assert_eq!(round_trip(shape.clone()), shape);
        let json = serde_json::to_value(&shape).unwrap();
        assert_eq!(json["kind"], "lasso");
    }

    #[test]
    fn arrow_shape_serde_round_trips() {
        let shape = AnnotationShape::Arrow {
            start: p(0.12, 0.30),
            end: p(0.55, 0.62),
        };
        assert_eq!(round_trip(shape.clone()), shape);
        let json = serde_json::to_value(&shape).unwrap();
        assert_eq!(json["kind"], "arrow");
    }

    #[test]
    fn label_shape_serde_round_trips() {
        let shape = AnnotationShape::Label {
            position: p(0.5, 0.5),
            text: "cut here".into(),
        };
        assert_eq!(round_trip(shape.clone()), shape);
        let json = serde_json::to_value(&shape).unwrap();
        assert_eq!(json["kind"], "label");
    }

    #[test]
    fn annotation_linked_range_us_round_trips_some_and_none() {
        let with_range = Annotation {
            id: "a1".into(),
            shape: AnnotationShape::Arrow {
                start: p(0.0, 0.0),
                end: p(1.0, 1.0),
            },
            linked_range_us: Some((1_000_000, 1_000_000)),
            space: AnnotationSpace::FrameLinked,
        };
        let json = serde_json::to_string(&with_range).unwrap();
        let back: Annotation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with_range);

        let without_range = Annotation {
            linked_range_us: None,
            ..with_range
        };
        let json = serde_json::to_string(&without_range).unwrap();
        let back: Annotation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, without_range);
    }

    #[test]
    fn project_without_canvas_key_deserializes_with_empty_annotations() {
        // An old (pre-Phase-13) snapshot has NO "canvas" key at all. The
        // `#[serde(default)]` on `Project.canvas` must keep it loadable —
        // the exact pattern already used for playback/preview_mode.
        let mut json = serde_json::to_value(Project::new()).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("canvas")
            .expect("Project serializes a canvas key");
        let project: Project = serde_json::from_value(json).unwrap();
        assert!(project.canvas.annotations.is_empty());
    }

    #[test]
    fn project_with_canvas_key_round_trips_annotations_exactly() {
        let mut project = Project::new();
        project.canvas.annotations.push(Annotation {
            id: "a1".into(),
            shape: AnnotationShape::Lasso {
                points: vec![p(0.2, 0.2), p(0.4, 0.2), p(0.4, 0.4)],
            },
            linked_range_us: Some((2_500_000, 2_500_000)),
            space: AnnotationSpace::FrameLinked,
        });
        project.canvas.annotations.push(Annotation {
            id: "a2".into(),
            shape: AnnotationShape::Label {
                position: p(0.9, 0.1),
                text: "trim this".into(),
            },
            linked_range_us: None,
            space: AnnotationSpace::Whiteboard,
        });
        let json = serde_json::to_string(&project).unwrap();
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(back.canvas, project.canvas);
        assert_eq!(back, project);
    }

    #[test]
    fn kind_label_names_all_four_variants() {
        assert_eq!(
            AnnotationShape::Stroke { points: vec![] }.kind_label(),
            "stroke"
        );
        assert_eq!(
            AnnotationShape::Lasso { points: vec![] }.kind_label(),
            "lasso"
        );
        assert_eq!(
            AnnotationShape::Arrow {
                start: p(0.0, 0.0),
                end: p(1.0, 1.0)
            }
            .kind_label(),
            "arrow"
        );
        assert_eq!(
            AnnotationShape::Label {
                position: p(0.5, 0.5),
                text: String::new()
            }
            .kind_label(),
            "label"
        );
    }

    // --- Phase 14.2 (D-06): AnnotationSpace discriminator ---------------

    #[test]
    fn annotation_space_default_is_frame_linked() {
        assert_eq!(AnnotationSpace::default(), AnnotationSpace::FrameLinked);
    }

    #[test]
    fn annotation_without_space_key_deserializes_as_frame_linked() {
        // A pre-14.2 snapshot has NO "space" key at all. The
        // `#[serde(default)]` on `Annotation.space` must keep it loadable —
        // the exact backward-compat pattern used for `Project.canvas` and
        // `AnnotationView.range_*`.
        let json = serde_json::json!({
            "id": "a1",
            "shape": { "kind": "stroke", "points": [{ "x": 0.1, "y": 0.2 }, { "x": 0.3, "y": 0.4 }] },
            "linked_range_us": null
        });
        let ann: Annotation = serde_json::from_value(json).expect("legacy annotation loads");
        assert_eq!(
            ann.space,
            AnnotationSpace::FrameLinked,
            "a space-less annotation must default to FrameLinked"
        );
    }

    #[test]
    fn whiteboard_annotation_serializes_snake_case_and_round_trips() {
        let ann = Annotation {
            id: "w1".into(),
            shape: AnnotationShape::Stroke {
                points: vec![p(0.1, 0.2), p(0.3, 0.4)],
            },
            linked_range_us: None,
            space: AnnotationSpace::Whiteboard,
        };
        let json = serde_json::to_string(&ann).unwrap();
        assert!(
            json.contains("\"space\":\"whiteboard\""),
            "Whiteboard space must serialize snake_case:\n{json}"
        );
        let back: Annotation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ann, "explicit space must round-trip byte-for-byte");
    }

    #[test]
    fn frame_linked_annotation_serializes_snake_case() {
        let ann = Annotation {
            id: "f1".into(),
            shape: AnnotationShape::Stroke {
                points: vec![p(0.1, 0.2), p(0.3, 0.4)],
            },
            linked_range_us: Some((1_000_000, 1_000_000)),
            space: AnnotationSpace::FrameLinked,
        };
        let json = serde_json::to_string(&ann).unwrap();
        assert!(
            json.contains("\"space\":\"frame_linked\""),
            "FrameLinked space must serialize snake_case:\n{json}"
        );
        let back: Annotation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ann);
    }
}
