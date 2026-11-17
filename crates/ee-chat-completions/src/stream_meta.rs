//! Stream framing checks and the typed idle-chunk timeout.
//!
//! Kept apart from the transport body so the transport stays under the
//! repository's file-size threshold.

use ee_agent_acp_server::{Delivery, ProviderError};
use reqwest::header::{CONTENT_TYPE, HeaderMap};

/// Typed idle-chunk timeout.
#[must_use]
pub fn chunk_timeout(label: &str) -> ProviderError {
    ProviderError::Transport {
        delivery: Delivery::Ambiguous,
        code: Some(String::from("TimeoutChunk")),
        detail: format!("{label} stream produced no data within the chunk budget"),
    }
}

/// Whether a response body is an SSE stream.
#[must_use]
pub fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with("text/event-stream"))
}
