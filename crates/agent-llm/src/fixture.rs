//! `FixtureTransport` — a scripted, zero-network `LlmTransport` implementation.
//!
//! Every later deterministic test in this phase (Plans 02/03/04) drives the
//! orchestration loop with this instead of `AnthropicTransport`: it replays a
//! canned `Vec<MessagesResponse>` in order and records every `MessagesRequest`
//! actually built and sent, so a test can assert request SHAPE, not just
//! response handling. It requires no `ANTHROPIC_API_KEY` and touches no network.

use std::cell::RefCell;

use crate::transport::{LlmError, LlmTransport, MessagesRequest, MessagesResponse};

/// Canned response script for deterministic, zero-network tests. `send()`
/// pops the next scripted response in order. `&self` (not `&mut self`,
/// matching the `LlmTransport` trait) via interior mutability -- single-
/// threaded test use only, never shared across real concurrent callers.
pub struct FixtureTransport {
    script: RefCell<std::vec::IntoIter<MessagesResponse>>,
    requests_seen: RefCell<Vec<MessagesRequest>>,
}

impl FixtureTransport {
    pub fn new(script: Vec<MessagesResponse>) -> Self {
        Self {
            script: RefCell::new(script.into_iter()),
            requests_seen: RefCell::new(Vec::new()),
        }
    }

    /// Every request actually built and sent so far, in order.
    pub fn requests_seen(&self) -> Vec<MessagesRequest> {
        self.requests_seen.borrow().clone()
    }
}

impl LlmTransport for FixtureTransport {
    async fn send(&self, request: &MessagesRequest) -> Result<MessagesResponse, LlmError> {
        self.requests_seen.borrow_mut().push(request.clone());
        self.script
            .borrow_mut()
            .next()
            .ok_or_else(|| LlmError::Api("FixtureTransport script exhausted".to_string()))
    }
}
