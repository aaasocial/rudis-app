//! The canonical, single-source-of-truth Anthropic Messages API wire types plus
//! the `LlmTransport` seam (real `reqwest`-based `AnthropicTransport` +
//! test-only `FixtureTransport`, the latter in `crate::fixture`).
//!
//! Anthropic's OWN wire field names are already snake_case (`model`,
//! `max_tokens`, `tool_use_id`, `cache_control`, `stop_reason`, `input_tokens`,
//! `cache_read_input_tokens`, ...) so only the tag names need
//! `#[serde(rename = "type")]` / `rename_all = "snake_case"`.
//!
//! `AnthropicTransport` is compiled but NEVER exercised by `cargo test` — every
//! automated test in this phase drives `FixtureTransport` (zero network, zero
//! `ANTHROPIC_API_KEY`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: Vec<SystemBlock>,
    pub tools: Vec<ToolDef>,
    pub messages: Vec<MessageParam>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "text"
    pub text: String,
    // Some(..) ONLY on the RULEBOOK block specifically (which may or may not
    // be literally last: `run_turn_with_context` places a second, per-turn
    // dynamic block AFTER it with NO cache_control — Anthropic's cache
    // lookback is per-breakpoint and backward-only, so later uncached content
    // never invalidates the rulebook block's cache hit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "ephemeral"
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>, // Some(..) ONLY on the LAST tool
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageParam {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    Image {
        source: ImageSource,
    },
    // Extended/interleaved "thinking" reasoning blocks some models (e.g. Sonnet 5)
    // return. Not actionable for the Store, but MUST round-trip: `run_turn` echoes
    // assistant content verbatim, so keeping these Serialize+Deserialize preserves
    // the block (and its `signature`) across a multi-round tool turn. Unknown
    // VARIANTS (not just fields) hard-fail an internally-tagged enum, so a missing
    // arm here breaks the whole response parse.
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
}

/// One block inside a `ToolResult.content` array (Phase 16, TOOL-06/D-04).
/// Distinct from `ContentBlock` (which also has an `Image` variant, used for
/// USER-turn content) because `tool_result.content` is its own documented
/// Anthropic union — reusing `ContentBlock` directly would let a `ToolUse`/
/// `ToolResult` block illegally nest inside another `ToolResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultBlock {
    Text { text: String },
    Image { source: ImageSource },
}

/// The `source` payload of an `image` content block, exactly as documented for
/// the Anthropic Messages API vision capability (Phase 13). Constructed ONLY by
/// `crate::vision::image_content_block_png`/`image_content_block_jpeg` —
/// callers never build this directly.
///
/// The constant fields stay `&'static str` (matching the sibling
/// `SystemBlock`/`CacheControl` wire types), but because `ImageSource` lives
/// inside the OWNED `ContentBlock` enum (whose derived `Deserialize` must work
/// for ANY input lifetime — serde's implicit `&str` borrowing would pin the
/// impl to `'de: 'static`), `Deserialize` is implemented by hand below: the
/// documented literal wire values map back onto their `'static` equivalents
/// and anything else is rejected loudly (never silently coerced).
#[derive(Debug, Clone, Serialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub kind: &'static str,       // always "base64"
    pub media_type: &'static str, // "image/png" | "image/jpeg" (construction sites choose the literal)
    pub data: String,             // base64-encoded image bytes
}

impl<'de> Deserialize<'de> for ImageSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawImageSource {
            #[serde(rename = "type")]
            kind: String,
            media_type: String,
            data: String,
        }
        let raw = RawImageSource::deserialize(deserializer)?;
        let kind = match raw.kind.as_str() {
            "base64" => "base64",
            other => return Err(serde::de::Error::unknown_variant(other, &["base64"])),
        };
        let media_type = match raw.media_type.as_str() {
            "image/png" => "image/png",
            "image/jpeg" => "image/jpeg",
            other => return Err(serde::de::Error::unknown_variant(other, &["image/png", "image/jpeg"])),
        };
        Ok(ImageSource {
            kind,
            media_type,
            data: raw.data,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>, // "tool_use" | "end_turn" | ...
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64, // SC-4's LIVE evidence field
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// Phase 69 (D-69-09): the user-facing "no key" text points at Settings (the
/// in-app key entry), not at an environment variable. Never echoes a key.
pub const NO_API_KEY_MESSAGE: &str = "No Anthropic API key — add one in Settings";

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("{}", NO_API_KEY_MESSAGE)]
    NoApiKey,
    #[error("http error: {0}")]
    Http(String),
    #[error("anthropic API error: {0}")]
    Api(String),
    /// [bug/agent-history-413] `run_turn_inner`'s budget guard: the
    /// serialized request STILL exceeds the size budget even after stripping
    /// every image block from history as a last resort — the bloat isn't
    /// images (Layer 1/2 already shrink/prune those), so no further automatic
    /// recovery is possible. Returned INSTEAD of calling `transport.send()`,
    /// so the caller gets an actionable message rather than the Anthropic API's
    /// opaque, unrecoverable `413 request_too_large`.
    #[error(
        "request too large: {bytes} bytes even after stripping every image from history \
         (the Anthropic Messages API caps the request body at 32MB) — start a new chat to continue"
    )]
    RequestTooLarge { bytes: usize },
}

/// Returns true iff `ANTHROPIC_API_KEY` is set in the process environment.
/// Used by `AnthropicTransport::from_env` AND by the host's `agent_status`
/// command (`app_core::auth::run_agent_status`; Plan 04) — the ONE place that
/// reads this env var name.
pub fn api_key_configured() -> bool {
    std::env::var("ANTHROPIC_API_KEY").is_ok()
}

// `LlmTransport` is used ONLY generically (`fn run_turn<T: LlmTransport>(...)`
// in Plan 03), never as `dyn LlmTransport`, so native `async fn` in a trait
// (stable since Rust 1.75) is exactly the case this lint says is safe to
// suppress ("use the trait only in your own code... do not care about auto
// traits like `Send`"). No `async-trait` dependency is needed.
#[allow(async_fn_in_trait)]
pub trait LlmTransport {
    async fn send(&self, request: &MessagesRequest) -> Result<MessagesResponse, LlmError>;
}

/// The real, key-gated network transport — the ONE network egress point in the
/// whole workspace (`reqwest` POST to `api.anthropic.com/v1/messages`).
///
/// T-12-01 (Information Disclosure): `api_key` is a PRIVATE field, the struct
/// itself is deliberately NOT `Debug`/`Serialize`-derived (nothing here can leak
/// the key into a log line), and `from_env()` returns `None` rather than
/// surfacing WHY a key lookup failed.
pub struct AnthropicTransport {
    client: reqwest::Client,
    api_key: String,
}

impl AnthropicTransport {
    /// `None` (never panics) when `ANTHROPIC_API_KEY` is absent — the caller
    /// surfaces a "disconnected" Chat state instead of failing app startup.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok()?;
        Some(Self {
            client: reqwest::Client::new(),
            api_key,
        })
    }

    /// BYO-key resolution (Phase 15, AUTH-02): the `store`'s key WINS; only when
    /// the store is empty (or errors) does it fall back to `ANTHROPIC_API_KEY`
    /// (Phase 12 dev/live-UAT convenience, unchanged). `Option::or_else`
    /// short-circuits — when the store has a key, `ANTHROPIC_API_KEY` is never
    /// even read, which is exactly what makes this safe to unit-test with a
    /// seeded `InMemoryKeyStore` regardless of ambient process env. `None`
    /// (never panics) when NEITHER source has a key.
    pub fn connect(store: &dyn crate::key_store::KeyStore) -> Option<Self> {
        let api_key = store
            .get()
            .ok()
            .flatten()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())?;
        Some(Self {
            client: reqwest::Client::new(),
            api_key,
        })
    }
}

/// True iff the `store` has a key OR (fallback) `ANTHROPIC_API_KEY` is set —
/// the BYO-key flavor of [`api_key_configured`], read by the host's
/// `agent_status` command (`app_core::auth::run_agent_status`; Phase 15). Same short-circuit as
/// [`AnthropicTransport::connect`]: a seeded store answers `true` without ever
/// reading the ambient env, so it is deterministic under parallel `cargo test`.
pub fn key_configured(store: &dyn crate::key_store::KeyStore) -> bool {
    matches!(store.get(), Ok(Some(_))) || std::env::var("ANTHROPIC_API_KEY").is_ok()
}

impl LlmTransport for AnthropicTransport {
    async fn send(&self, request: &MessagesRequest) -> Result<MessagesResponse, LlmError> {
        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(request)
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api(body));
        }
        resp.json().await.map_err(|e| LlmError::Http(e.to_string()))
    }
}

#[cfg(test)]
mod connect_tests {
    use super::*;
    use crate::key_store::InMemoryKeyStore;

    /// A well-formed but FAKE key — never a real credential.
    const FAKE_KEY: &str = "sk-ant-api03-FAKE0000000000000000000000000000000000";

    #[test]
    fn connect_builds_transport_from_a_seeded_store() {
        // A store seeded with a key wins over ambient env (Option::or_else
        // short-circuits before ANTHROPIC_API_KEY is even read), so this is
        // deterministic regardless of the test host's environment.
        let store = InMemoryKeyStore::seeded(FAKE_KEY);
        assert!(
            AnthropicTransport::connect(&store).is_some(),
            "a seeded store builds a transport"
        );
    }

    #[test]
    fn key_configured_true_for_a_seeded_store() {
        let store = InMemoryKeyStore::seeded(FAKE_KEY);
        assert!(key_configured(&store), "a seeded store reports configured");
    }

    #[test]
    fn no_api_key_error_points_at_settings() {
        assert_eq!(LlmError::NoApiKey.to_string(), NO_API_KEY_MESSAGE);
        assert!(NO_API_KEY_MESSAGE.contains("Settings"));
    }
}
