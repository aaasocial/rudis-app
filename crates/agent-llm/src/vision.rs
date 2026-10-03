//! The vision-snapshot wire helpers (Phase 13, CANV-01/SC-4 image half; Phase
//! 16, TOOL-06/D-04 tool_result blocks): wraps raw encoded image bytes into
//! the documented Anthropic content-block shapes.
//!
//! This module is the ONE place `base64` is used in the whole workspace
//! (PROVENANCE Entry 8). `image_content_block_png`/`image_content_block_jpeg`
//! are the public constructors the host's vision-snapshot assembly (Wave 3;
//! `crates/app-core/src/agent_turn.rs`) calls after compositing the canvas over
//! the current frame; `text_tool_result`/
//! `image_tool_result` (Phase 16) are the ONLY constructors of
//! `ToolResult.content` block lists — callers never construct `ImageSource`
//! directly.
//!
//! [bug/agent-history-413] `image_content_block_jpeg` exists because
//! `AgentSession.history` (host-owned; `crates/app-core`) is never pruned by turn count and every
//! request re-sends the WHOLE history verbatim — a lossless PNG frame
//! snapshot (~2-4MB) is the dominant contributor to the Anthropic Messages
//! API's 32MB HTTP request-body cap (`413 request_too_large`, independent of
//! any token/context budget). The CHAT-HISTORY vision snapshot uses JPEG
//! (the host's `build_vision_snapshot_jpeg`, ~150-250KB); the WHITEBOARD
//! snapshot (flat-color line art, where JPEG ringing smears strokes — see
//! the `canvas-sketch-generate-picture-garbled-output` /
//! `generate-me-rectangle-ignores-drawn-geometry` debug sessions) and the
//! GEN-10 reference-image conditioning seam both deliberately stay PNG via
//! `image_content_block_png`.
//!
//! Ordering contracts (proven by `tests/request_shape.rs` and
//! `tests/tool_result_image.rs`): the USER-turn vision snapshot places its
//! image block BEFORE the text block, per Anthropic's documented vision
//! guidance (platform.claude.com/docs/en/build-with-claude/vision); a
//! `tool_result`'s content places its text block FIRST, per Anthropic's
//! documented tool_result example ordering — the two are deliberately
//! distinct.

use base64::{engine::general_purpose::STANDARD, Engine as _};

use crate::transport::{ContentBlock, ImageSource, ToolResultBlock};

/// Wrap raw PNG bytes into the Anthropic `image` content-block shape
/// (base64-encoded). Used for the WHITEBOARD snapshot (flat-color line art —
/// lossless matters, see the module doc) and anywhere else a caller has
/// already-PNG-encoded bytes.
pub fn image_content_block_png(png_bytes: &[u8]) -> ContentBlock {
    ContentBlock::Image {
        source: ImageSource {
            kind: "base64",
            media_type: "image/png",
            data: STANDARD.encode(png_bytes),
        },
    }
}

/// Wrap raw JPEG bytes into the Anthropic `image` content-block shape
/// (base64-encoded). [bug/agent-history-413] Used for the CHAT-HISTORY frame
/// vision snapshot — the one PERMANENTLY-persisted, every-turn-resent image
/// block where lossless fidelity is not worth a 10-20x size cost (see the
/// module doc). `jpeg_bytes` must already be JPEG-encoded (e.g. via
/// `engine::encode_jpeg_bytes`) — this function only base64-wraps them.
pub fn image_content_block_jpeg(jpeg_bytes: &[u8]) -> ContentBlock {
    ContentBlock::Image {
        source: ImageSource {
            kind: "base64",
            media_type: "image/jpeg",
            data: STANDARD.encode(jpeg_bytes),
        },
    }
}

/// Wrap a plain string as a single-block `ToolResult.content` — the
/// zero-behavior-change replacement for every existing bare-`String`
/// tool_result construction site (Phase 16, TOOL-06/D-04's plumbing;
/// no image involved here). Used by BOTH crates/agent-llm/src/turn.rs
/// and `app_core::agent_turn::build_user_turn` (a separate workspace
/// member that constructs ContentBlock::ToolResult directly).
pub fn text_tool_result(text: impl Into<String>) -> Vec<ToolResultBlock> {
    vec![ToolResultBlock::Text { text: text.into() }]
}

/// Wrap a text summary + a real JPEG image into a `ToolResult.content` array,
/// text block FIRST per Anthropic's documented tool_result example ordering
/// (distinct from the user-turn vision-snapshot ordering, which is
/// image-before-text). `jpeg_bytes` must already be JPEG-encoded (e.g. via
/// `engine::encode_jpeg_bytes`) — this function only base64-wraps them.
pub fn image_tool_result(text: impl Into<String>, jpeg_bytes: &[u8]) -> Vec<ToolResultBlock> {
    vec![
        ToolResultBlock::Text { text: text.into() },
        ToolResultBlock::Image {
            source: ImageSource {
                kind: "base64",
                media_type: "image/jpeg",
                data: STANDARD.encode(jpeg_bytes),
            },
        },
    ]
}

/// Wrap a text summary + MULTIPLE real JPEG images into a `ToolResult.content`
/// array -- the storyboard-mode twin of `image_tool_result` (Phase 21,
/// EYES-02): text block FIRST, then one `Image` block per JPEG, in the SAME
/// order supplied. Each entry must already be JPEG-encoded (e.g. via
/// `engine::encode_jpeg_bytes`).
pub fn images_tool_result(text: impl Into<String>, jpegs: &[Vec<u8>]) -> Vec<ToolResultBlock> {
    let mut blocks = vec![ToolResultBlock::Text { text: text.into() }];
    for jpeg in jpegs {
        blocks.push(ToolResultBlock::Image {
            source: ImageSource {
                kind: "base64",
                media_type: "image/jpeg",
                data: STANDARD.encode(jpeg),
            },
        });
    }
    blocks
}
